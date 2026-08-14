# VoltPanel REST API

The control-plane API is JSON over HTTP on the panel's listen port
(`8080` by default). This document covers the cross-cutting conventions;
the authoritative route list lives in the generated OpenAPI document served
at [`/api/meta/openapi.json`](#discovery).

## Discovery

| Endpoint | Description |
|---|---|
| `GET /api/meta` | Public panel metadata: `version`, `api.version`, `node_protocol`, feature flags, resource limits, `rate_limit_per_min`. No secrets. |
| `GET /api/meta/openapi.json` | OpenAPI 3 document: auth schemes, error envelope, and the representative route surface. |

Clients should fetch `/api/meta` first and gate feature-specific calls on
the returned `features` flags (for example, do not call backup endpoints
when `features.backups` is `false`).

## Authentication

Two mutually exclusive credentials; endpoints accept either:

1. **Session cookie** — `POST /api/login` sets the `vp_session` cookie.
   Cookie-authenticated *mutations* must also send an `Origin` header that
   matches the panel's own host (anti-CSRF); API clients that only read are
   unaffected.
2. **API key** — `Authorization: Bearer vp_...` (requires the `api_keys`
   feature). Keys are scoped: server-scoped keys are narrowed to their
   servers and capabilities, and no key can call admin endpoints unless it
   has full authority.

Unauthenticated requests answer `401 {"error": "...", "status": 401}`.
Admin endpoints additionally require a root-admin account.

## Errors

Every error response uses the same envelope:

```json
{ "error": "human-readable summary", "status": 409 }
```

Common statuses:

| Status | Meaning |
|---|---|
| 400 | Malformed body, invalid parameter, or business-rule violation |
| 401 | Not logged in / invalid session or API key |
| 403 | Missing capability, admin-only, or failed same-origin check |
| 404 | Unknown resource |
| 409 | Conflict (for example: reused idempotency key with a different body) |
| 413 | Body exceeds `web.max_body_mb` |
| 429 | Rate limit exceeded — see below |
| 500 | Internal error (the message is deliberately generic) |

Success responses are `200`/`201` and return the payload directly; list
endpoints wrap in `{"data": [...]}`, and paginated lists add `total`,
`page`, and `limit`.

## Request id and rate-limit headers

**Every response** — success, error, and fallback alike — carries:

```
x-volt-request-id: <uuid>
```

This is the correlation id logged by the panel for that request; include it
when reporting a problem. It is minted fresh per request and never echoed
from the client.

**Mutations** (every non-`GET`/`HEAD`/`OPTIONS` `/api/*` request except
node enrollment/heartbeat) are rate-limited per client IP with a token
bucket of `security.rate_limit_per_min` tokens per minute. Responses to
approved mutations and to `429`s carry standard rate-limit headers:

```
RateLimit-Limit: 120        # security.rate_limit_per_min
RateLimit-Reset: 1752768000 # epoch seconds of the next 60s window boundary
RateLimit-Remaining: 0      # only on 429; omitted otherwise
```

`RateLimit-Reset` is an approximation: the panel's bucket refills against a
fixed 60-second window, and `Reset` is that window's next boundary. The
bucket's exact depth is not exposed, so `RateLimit-Remaining` is only set
when it is provable — `0` on a rejected `429`. On `429`, back off at least
until `RateLimit-Reset`.

## Idempotency

High-risk mutations accept an `Idempotency-Key` header so a retry after a
timeout or connection drop cannot double-create:

| Endpoint | Method |
|---|---|
| `/api/servers` (create) | `POST` |
| `/api/servers/:id/backups` (create) | `POST` |
| `/api/backups/:id/restore` | `POST` |
| `/api/schedules/:id/run` (run-now) | `POST` |

Semantics:

- The key is scoped to **user + method + path**: the same key on a
  different endpoint or account is a distinct operation.
- The first request with a key executes normally; a **2xx JSON response**
  is cached for **10 minutes** (bounded in memory at 10 000 entries).
- **Only successful 2xx responses are cached.** 4xx rejections (validation
  errors, conflicts), 5xx failures, and streaming/binary responses are
  never cached, so a rejected or failed request never poisons the key — a
  retry with the same key simply re-executes.
- A later request with the same user/method/path/key **replays the cached
  response verbatim** without re-running the operation — use one fresh key
  per intended operation and reuse it only for retries.
- Reusing a key with a *different* request body answers `409`.
- **Concurrent requests with the same key are serialized, not
  double-executed**: the first installs an in-flight slot and runs the
  handler; followers with the same body wait for it (bounded at 60 seconds)
  and then replay the cached response, so the handler runs exactly once. A
  different body while the request is in flight answers `409` immediately.
  If the owner's response is not cacheable or the owner is aborted, one
  waiting follower re-checks and re-executes, so every client is answered
  and a transient failure cannot wedge the key.
- When the in-memory table is at the 10 000-entry ceiling and pruning
  expired entries does not make room, the new request is **not recorded**:
  it executes exactly once and its response is not cached, so a retry with
  the same key would execute again. Existing live entries are never
  evicted, so the serialization guarantee for already-recorded keys is
  unaffected. Under pathological volume, treat the key as advisory for
  *new* keys until entries expire.

## Pagination

List endpoints paginate with query parameters:

- `page` — 1-based page number, default `1`.
- `limit` — page size, default `50`; endpoint-specific caps (`100` for
  schedule runs, `200` for servers and webhook deliveries, `500` for
  activity).

Paginated responses include the page metadata alongside the data, e.g. the
servers list returns `{"data": [...], "total": N, "page": P, "limit": L}`.

## Streaming endpoints

These return a stream, not a JSON document, and are exempt from
idempotency caching (they still carry `x-volt-request-id`):

- `GET /api/servers/:id/console/stream` — live console output (SSE-style
  event stream).
- `GET /api/servers/:id/console/log` — console log file.
- `GET /api/backups/:id/download` — backup archive (`application/gzip` or
  `application/zip`, streamed).
- `GET /api/servers/:id/files/download` — file download stream.
- `GET /api/notifications/stream` — live admin notifications (SSE,
  `text/event-stream`); the client's `EventSource` auto-reconnect restores
  the feed after a disconnect.
- `GET /api/servers/:id/databases/:name/export` — consistent SQLite
  snapshot of one database (`application/octet-stream`, streamed; the
  temporary snapshot is deleted when the download completes or aborts).

Interrupted downloads are safe to retry; nothing is re-executed on the
server until the download completes.

## Conventions

- All bodies are UTF-8 JSON; request bodies must declare
  `Content-Type: application/json` (multipart file uploads excepted).
- Resource ids are integers; timestamps are ISO-8601 in UTC.
- Unknown query parameters are ignored; unknown JSON fields are rejected
  (`serde(deny_unknown_fields)`).
- Node endpoints under `/api/node/*` are HMAC-signed daemon traffic and are
  not subject to the browser-session rules above.
