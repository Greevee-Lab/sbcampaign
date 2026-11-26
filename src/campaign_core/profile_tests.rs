// SPDX-License-Identifier: AGPL-3.0-only
//! Core API against PostgreSQL (`sql/fork/core_api.sql`): display names,
//! idempotency, audit, suspension, access mirror. Synthetic data only; skipped
//! without `SBCAMPAIGN_TEST_DATABASE_URL` (see `tests.rs`).

use super::{
    SbCampaignCore,
    tests::{core_pool, synthetic_account, test_pool},
};
use crate::core_api::{
    Provider,
    names::NamePolicy,
    net::{
        access::{Access, Scope},
        pb,
        service::{Caller, CoreApi, method},
    },
    profile::{AccessRecord, ProfileCore},
};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

/// Letters whose look-alike skeleton is themselves (no i l o r n m v w).
const TAG_LETTERS: &[u8] = b"abcdefghjkpqstuxyz";

/// A 6-letter tag unique per run, so names never collide with earlier runs.
fn tag(test: u64) -> String {
    let mut n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 ^ (test << 40);
    (0..6)
        .map(|_| {
            let c = TAG_LETTERS[(n % TAG_LETTERS.len() as u64) as usize] as char;
            n /= TAG_LETTERS.len() as u64;
            c
        })
        .collect()
}

struct Env {
    api: CoreApi<SbCampaignCore>,
    pool: sqlx::PgPool,
    clock: Arc<AtomicI64>,
    steelnet: Caller,
    admin: Caller,
    access_name: String,
}

fn caller(name: &str, provider: Option<Provider>, scopes: &[Scope]) -> Caller {
    Caller(Some(Arc::new(Access {
        name: name.to_owned(),
        provider,
        cert_sha256: "00".repeat(32),
        scopes: scopes.to_vec(),
        rate_per_second: 1000.0,
        burst: 1000.0,
    })))
}

async fn env(test: u64) -> Option<Env> {
    let campaign = test_pool().await?;
    // Every table these tests read is a core table.
    let pool = core_pool(&campaign).await;
    let clock = Arc::new(AtomicI64::new(chrono::Utc::now().timestamp()));
    let c2 = clock.clone();
    let api = CoreApi::new(
        Arc::new(SbCampaignCore::new(campaign, pool.clone())),
        NamePolicy::default(),
        Arc::new(move || c2.load(Ordering::SeqCst)),
    );
    let access_name = format!("test-{}", tag(test));
    Some(Env {
        api,
        pool,
        clock,
        steelnet: caller(&access_name, Some(Provider::Steelbound), &[Scope::IdentityWrite, Scope::ProfileWrite, Scope::ReadPublic]),
        admin: caller(&format!("{access_name}-admin"), None, &[Scope::Admin]),
        access_name,
    })
}

impl Env {
    async fn resolve(&self, account: u32) -> pb::ResolveIdentityResponse {
        self.api
            .resolve_identity(&self.steelnet, pb::ResolveIdentityRequest { external_id: account.to_string(), idempotency_key: String::new() })
            .await
            .unwrap()
    }

    async fn set(&self, player_id: &str, name: &str, key: &str) -> pb::SetDisplayNameResponse {
        self.api
            .set_display_name(
                &self.steelnet,
                pb::SetDisplayNameRequest { player_id: player_id.into(), name: name.into(), idempotency_key: key.into() },
            )
            .await
            .unwrap()
    }

    fn advance_days(&self, d: i64) {
        self.clock.fetch_add(d * 86_400, Ordering::SeqCst);
    }
}

fn st(r: &pb::SetDisplayNameResponse) -> pb::DisplayNameStatus {
    r.status()
}

#[tokio::test]
async fn db_core_api_display_names() {
    let Some(e) = env(1).await else { return };
    use pb::DisplayNameStatus as D;
    let t = tag(11);
    let a = e.resolve(synthetic_account(11)).await;
    let b = e.resolve(synthetic_account(12)).await;
    assert!(a.created && a.display_name.is_empty());
    let name = format!("Al{t}");
    let k = |n: u32| format!("{t}-key-{n:03}");

    assert_eq!(st(&e.set(&a.player_id, &name, &k(1)).await), D::Ok);
    assert_eq!(e.resolve(synthetic_account(11)).await.display_name, "", "different synthetic account");
    assert_eq!(st(&e.set(&b.player_id, &name.to_uppercase(), &k(2)).await), D::Taken);
    assert_eq!(st(&e.set(&b.player_id, &format!("AI{t}"), &k(3)).await), D::Similar);
    assert_eq!(st(&e.set(&b.player_id, &format!("A1{t}"), &k(4)).await), D::Similar);
    assert_eq!(st(&e.set(&b.player_id, &format!("Adm1n{t}"), &k(5)).await), D::Blocked, "seeded blocklist");
    assert_eq!(st(&e.set(&b.player_id, "9lives", &k(6)).await), D::InvalidCharacters);
    assert_eq!(st(&e.set(&a.player_id, &name, &k(7)).await), D::Unchanged);

    // Change limit, then reservation of the old name.
    let r = e.set(&a.player_id, &format!("B{t}"), &k(8)).await;
    assert_eq!(st(&r), D::ChangeLimit);
    assert!(r.next_change_at > e.clock.load(Ordering::SeqCst));
    e.advance_days(8);
    assert_eq!(st(&e.set(&a.player_id, &format!("B{t}"), &k(9)).await), D::Ok);
    assert_eq!(st(&e.set(&b.player_id, &name, &k(10)).await), D::Reserved);
    let chk = e
        .api
        .check_display_name(&e.steelnet, pb::CheckDisplayNameRequest { name: name.clone(), player_id: b.player_id.clone() })
        .await
        .unwrap();
    assert_eq!((chk.valid, chk.available, chk.status()), (true, false, D::Reserved));
    e.advance_days(31);
    assert_eq!(st(&e.set(&b.player_id, &name, &k(11)).await), D::Ok);

    // ResolveIdentity reports the display name from the core.
    let again = e
        .api
        .resolve_identity(&e.steelnet, pb::ResolveIdentityRequest { external_id: b.external_id.clone(), idempotency_key: String::new() })
        .await
        .unwrap();
    assert_eq!(again.display_name, name);
    let p = e.api.get_player(&e.steelnet, pb::GetPlayerRequest { player_id: b.player_id.clone() }).await.unwrap().player.unwrap();
    assert_eq!(p.external_ids, vec![b.external_id.clone()]);
}

#[tokio::test]
async fn db_core_api_idempotency_and_audit() {
    let Some(e) = env(2).await else { return };
    let t = tag(21);
    let a = e.resolve(synthetic_account(21)).await;
    let key = format!("{t}-idem-0001");
    let first = e.set(&a.player_id, &format!("Q{t}"), &key).await;
    let again = e.set(&a.player_id, &format!("Q{t}"), &key).await;
    assert_eq!(first, again, "replay returns the stored response");
    let reuse = e
        .api
        .set_display_name(
            &e.steelnet,
            pb::SetDisplayNameRequest { player_id: a.player_id.clone(), name: format!("Z{t}"), idempotency_key: key.clone() },
        )
        .await;
    assert_eq!(reuse.unwrap_err().code.as_str(), "already_exists");

    // Audit: one row for the resolve (created), one for the name change; the
    // replay and the refused reuse leave no "ok" row; no contents anywhere.
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT action, provider_id, method, outcome, detail FROM audit_log WHERE access_id = $1 ORDER BY audit_id",
    )
    .bind(&e.access_name)
    .fetch_all(&e.pool)
    .await
    .unwrap();
    let summary: Vec<(String, String)> =
        rows.iter().map(|r| (r.2.clone().unwrap_or_default(), r.3.clone().unwrap_or_default())).collect();
    assert_eq!(
        summary,
        vec![
            (method::RESOLVE_IDENTITY.to_owned(), "created".to_owned()),
            (method::SET_DISPLAY_NAME.to_owned(), "ok".to_owned()),
        ]
    );
    for r in &rows {
        assert_eq!(r.0, "api_write");
        assert_eq!(r.1.as_deref(), Some("steelbound"));
        assert!(r.4.is_none(), "no detail text");
        assert!(!format!("{r:?}").contains(&t), "no request contents in the audit");
    }
}

#[tokio::test]
async fn db_core_api_suspension_and_access_mirror() {
    let Some(e) = env(3).await else { return };
    let acct = synthetic_account(31);
    let a = e.resolve(acct).await;
    assert!(!a.suspended);
    let r = e
        .api
        .set_identity_suspended(
            &e.admin,
            pb::SetIdentitySuspendedRequest {
                provider: "steelbound".into(),
                external_id: acct.to_string(),
                suspended: true,
                idempotency_key: format!("{}-susp-01", tag(31)),
            },
        )
        .await
        .unwrap();
    assert_eq!((r.identity_id.clone(), r.suspended), (a.identity_id.clone(), true));
    assert!(e.resolve(acct).await.suspended);

    // Access mirror: configured accesses are upserted, missing ones revoked.
    let store = e.api.store();
    let rec = |n: &str| AccessRecord {
        name: n.to_owned(),
        provider: Some(Provider::Steelbound),
        cert_sha256: "ab".repeat(32),
        scopes: vec!["read:public".into()],
    };
    let (n1, n2) = (format!("{}-m1", e.access_name), format!("{}-m2", e.access_name));
    store.sync_accesses(&[rec(&n1), rec(&n2)]).await.unwrap();
    store.sync_accesses(&[rec(&n1)]).await.unwrap();
    let revoked: Vec<(String, bool)> = sqlx::query_as(
        "SELECT access_id, revoked_at IS NOT NULL FROM api_access WHERE access_id = ANY($1) ORDER BY access_id",
    )
    .bind(vec![n1.clone(), n2.clone()])
    .fetch_all(&e.pool)
    .await
    .unwrap();
    assert_eq!(revoked, vec![(n1, false), (n2, true)]);
}
