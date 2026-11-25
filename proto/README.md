<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# steelcore.v1 – the Core API contract

The `.proto` files here are the single source of truth of the Core API. The
server generates its code from them at build time (`build.rs`); clients may
use any Protobuf/gRPC tool chain, or plain HTTPS with JSON.

| Service | Method | Scope | Write |
|---|---|---|---|
| `IdentityService` | `ResolveIdentity` | `identity:write` | yes (key optional) |
| `PlayerService` | `GetPlayer` | `read:public` | no |
| `PlayerService` | `CheckDisplayName` | `read:public` | no |
| `PlayerService` | `SetDisplayName` | `profile:write` | yes (key required) |
| `AdminService` | `SetIdentitySuspended` | `admin` | yes (key required) |

## Bindings

Both bindings run on the same mutual-TLS 1.3 port and behave identically.

- **gRPC:** HTTP/2, `content-type: application/grpc`, paths
  `/steelcore.v1.<Service>/<Method>`. Errors are gRPC status codes.
- **Connect-JSON** (unary Connect protocol): `POST /steelcore.v1.<Service>/<Method>`,
  `content-type: application/json`, body in the proto3 JSON mapping
  (lowerCamelCase names, `int64` as strings, enums by name, default values
  omitted, unknown fields ignored). Success: HTTP 200 with the response
  message. Error: Connect HTTP status (400, 401, 403, 404, 409, 429, 503)
  and `{"code": "<code>", "message": "..."}`.

## Errors

| Code | When |
|---|---|
| `unauthenticated` | the client certificate is from the steelcore CA but not registered |
| `permission_denied` | missing scope, or a player of another provider |
| `invalid_argument` | malformed id, idempotency key or request |
| `not_found` | unknown player or identity |
| `already_exists` | idempotency key reused for a different request |
| `aborted` | a call with the same idempotency key is still running |
| `resource_exhausted` | rate limit of the access |
| `failed_precondition` | provider disabled |
| `unavailable` | storage unavailable (retry with the same key) |

Display name refusals are not errors: `SetDisplayName` answers with a
`DisplayNameStatus` (`TAKEN`, `SIMILAR`, `RESERVED`, `CHANGE_LIMIT`, ...).
A connection without a client certificate, or with one of another CA, is
refused during the TLS handshake.

## Versioning

Fields are only added, never renumbered or retyped. Incompatible changes go
to a new package (`steelcore.v2`) served next to `v1`.
