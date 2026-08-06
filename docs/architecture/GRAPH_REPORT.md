# Graph Report - .  (2026-08-06)

## Corpus Check
- cluster-only mode — file stats not available

## Summary
- 1577 nodes · 5128 edges · 47 communities (46 shown, 1 thin omitted)
- Extraction: 99% EXTRACTED · 1% INFERRED · 0% AMBIGUOUS · INFERRED: 55 edges (avg confidence: 0.76)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `8654eacc`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- Community 0
- Community 1
- Community 2
- Community 3
- Community 4
- Community 5
- Community 6
- Community 7
- Community 8
- Community 9
- Community 10
- Community 11
- Community 12
- Community 13
- Community 14
- Community 15
- Community 16
- Community 17
- Community 18
- Community 19
- Community 20
- Community 21
- Community 22
- Community 23
- Community 24
- Community 25
- Community 26
- Community 27
- Community 28
- Community 29
- Community 30
- Community 31
- Community 32
- Community 33
- Community 34
- Community 35
- Community 36
- Community 37
- Community 38
- Community 39
- Community 40
- Community 41
- Community 42
- Community 43
- Community 44
- Community 45

## God Nodes (most connected - your core abstractions)
1. `AppState` - 179 edges
2. `Db` - 179 edges
3. `AuthUser` - 93 edges
4. `api()` - 83 edges
5. `toast()` - 80 edges
6. `t()` - 64 edges
7. `Server` - 56 edges
8. `AdminUser` - 54 edges
9. `esc()` - 39 edges
10. `data()` - 38 edges

## Surprising Connections (you probably didn't know these)
- `list()` --calls--> `data()`  [INFERRED]
  src/api/backups.rs → src/api/mod.rs
- `list()` --calls--> `data()`  [INFERRED]
  src/api/blueprints.rs → src/api/mod.rs
- `categories()` --calls--> `data()`  [INFERRED]
  src/api/blueprints.rs → src/api/mod.rs
- `revisions()` --calls--> `data()`  [INFERRED]
  src/api/blueprints.rs → src/api/mod.rs
- `revision_detail()` --calls--> `data()`  [INFERRED]
  src/api/blueprints.rs → src/api/mod.rs

## Import Cycles
- 1-file cycle: `src/api/mod.rs -> src/api/mod.rs`
- 2-file cycle: `src/api/mod.rs -> src/models.rs -> src/api/mod.rs`
- 2-file cycle: `src/models.rs -> src/services/keys.rs -> src/models.rs`
- 3-file cycle: `src/api/mod.rs -> src/models.rs -> src/services/keys.rs -> src/api/mod.rs`
- 3-file cycle: `src/auth.rs -> src/models.rs -> src/services/keys.rs -> src/auth.rs`
- 4-file cycle: `src/api/mod.rs -> src/models.rs -> src/services/keys.rs -> src/auth.rs -> src/api/mod.rs`

## Communities (47 total, 1 thin omitted)

### Community 0 - "Community 0"
Cohesion: 0.05
Nodes (135): adminBlueprintExport(), adminBlueprints(), adminCreateBlueprint(), adminCreateServer(), adminCreateUser(), adminDeleteBlueprint(), adminDeleteUser(), adminDelServer() (+127 more)

### Community 1 - "Community 1"
Cohesion: 0.05
Nodes (67): B, ClientBuilder, Method, body_hash(), canonical(), ConsoleCommand, ConsoleSnapshot, FileListRequest (+59 more)

### Community 2 - "Community 2"
Cohesion: 0.07
Nodes (45): CString, R, DaemonConfig, DaemonRuntime, dir_size(), ManagedProcess, now_ms(), openat2_beneath() (+37 more)

### Community 3 - "Community 3"
Cohesion: 0.06
Nodes (51): BTreeSet, Display, Err, Formatter, FromStr, IntoIterator, Iterator, Capability (+43 more)

### Community 4 - "Community 4"
Cohesion: 0.06
Nodes (46): Read, Monitor, node_stats(), NodeStats, Arc, AtomicBool, Config, Default (+38 more)

### Community 5 - "Community 5"
Cohesion: 0.11
Nodes (61): DResult, authenticated(), clear_console(), command(), config_arg(), console(), CursorQuery, DaemonError (+53 more)

### Community 6 - "Community 6"
Cohesion: 0.12
Nodes (65): FromRequestParts, Multipart, access_ok(), cleanup(), CleanupReq, create(), CreateReq, delete() (+57 more)

### Community 7 - "Community 7"
Cohesion: 0.12
Nodes (52): FileOptions, Server, authorizer_denies_attach_and_query_mutation(), db_dir(), drop(), exec(), install_authorizer(), list() (+44 more)

### Community 8 - "Community 8"
Cohesion: 0.09
Nodes (47): base32_decode(), base32_encode(), create_session(), generate_totp_secret(), hash_password(), hash_token(), percent_encode(), random_token() (+39 more)

### Community 9 - "Community 9"
Cohesion: 0.10
Nodes (37): Command, AtomicFlagGuard, AtomicFlagGuard<'a>, Cgroup, CgroupMetrics, delegated_cgroup_root(), enable_controllers(), enable_here() (+29 more)

### Community 10 - "Community 10"
Cohesion: 0.09
Nodes (42): CertificateDer, ClientConfig, CryptoProvider, DigitallySignedStruct, Future, HandshakeSignatureValid, Output, PrivateKeyDer (+34 more)

### Community 11 - "Community 11"
Cohesion: 0.11
Nodes (48): Db, add_schedule_task(), allocate_port(), ApiKey, apikey_from_row(), Backup, backup_from_row(), bump_rate_limit() (+40 more)

### Community 12 - "Community 12"
Cohesion: 0.17
Nodes (42): access_ok(), add_subuser(), AddSubuserReq, admin_list_all(), create(), CreateRollback, CreateServerReq, delete() (+34 more)

### Community 13 - "Community 13"
Cohesion: 0.15
Nodes (37): install-node.sh script, install-panel.sh script, arch_asset(), configure_caddy_import(), configure_certbot_ip_proxy(), configure_certbot_proxy(), configure_cloudflare_proxy(), die() (+29 more)

### Community 14 - "Community 14"
Cohesion: 0.13
Nodes (40): blueprint_row(), BlueprintImport, bp(), build_default_config(), content_doc(), default_runtime(), default_stop(), diff_revisions() (+32 more)

### Community 15 - "Community 15"
Cohesion: 0.13
Nodes (29): create(), delete(), find_by_host(), get(), get_inner(), list(), map_unique(), match_host() (+21 more)

### Community 16 - "Community 16"
Cohesion: 0.17
Nodes (34): ConnectInfo, admin_create_user(), admin_delete_user(), admin_list_users(), admin_revoke_session(), admin_sessions(), admin_update_user(), AdminUpdateUserReq (+26 more)

### Community 17 - "Community 17"
Cohesion: 0.14
Nodes (21): add_policy(), backoff_secs(), create_schedule(), next_run(), parse_cron(), policy(), RetryPolicy, Arc (+13 more)

### Community 18 - "Community 18"
Cohesion: 0.24
Nodes (28): create(), CreateNodeRequest, delete(), enroll(), EnrollmentRequest, get(), header(), heartbeat() (+20 more)

### Community 19 - "Community 19"
Cohesion: 0.24
Nodes (27): categories(), create(), CreateBlueprintReq, delete(), drift(), export(), ExportResp, get() (+19 more)

### Community 20 - "Community 20"
Cohesion: 0.14
Nodes (25): Grant, add_subuser(), full_authority_key_matches_a_session(), get_user(), get_user_by_email(), get_user_by_name(), list_subusers(), list_users() (+17 more)

### Community 21 - "Community 21"
Cohesion: 0.16
Nodes (26): Box, Event, Infallible, Pin, access_ok(), clear(), CommandReq, history() (+18 more)

### Community 22 - "Community 22"
Cohesion: 0.17
Nodes (18): ConsoleHub, ConsoleLine, drop_server(), eviction_past_capacity_reports_truncated(), history_after_known_id_returns_exactly_newer_lines(), id_newer_than_everything_returns_empty(), ids_are_monotonic_across_chunks(), LineBuf (+10 more)

### Community 23 - "Community 23"
Cohesion: 0.11
Nodes (25): all_settings(), audit(), AuditLog, Blueprint, BlueprintInput, create_blueprint(), get_blueprint(), get_server_vars() (+17 more)

### Community 24 - "Community 24"
Cohesion: 0.19
Nodes (17): ImageError, Parts, QrError, Rejection, ApiError, client_ip(), ok(), Error (+9 more)

### Community 25 - "Community 25"
Cohesion: 0.19
Nodes (21): downsample(), downsample_caps_output_at_max_points(), downsample_skips_empty_buckets(), downsample_under_limit_is_identity(), prune(), prune_deletes_old_rows(), range(), range_orders_asc_and_downsamples() (+13 more)

### Community 26 - "Community 26"
Cohesion: 0.33
Nodes (22): access_ok(), add_task(), AddTaskReq, create(), CreateScheduleReq, delete(), list(), remove_task() (+14 more)

### Community 27 - "Community 27"
Cohesion: 0.29
Nodes (22): allocations(), assign_port(), free_ports(), health(), isolation(), LimitOverride, live_stats(), node_stats() (+14 more)

### Community 28 - "Community 28"
Cohesion: 0.32
Nodes (21): AdminUser, create(), CreateReq, delete(), deliveries(), DeliveriesQuery, get(), list() (+13 more)

### Community 29 - "Community 29"
Cohesion: 0.16
Nodes (16): Limits, Config, Features, General, Limits, Paths, Default, Path (+8 more)

### Community 30 - "Community 30"
Cohesion: 0.17
Nodes (12): FromRef, AppState, Arc<ConsoleHub>, Arc<Monitor>, Arc<proc::Notifier>, Arc<proc::ProcManager>, Config, Arc (+4 more)

### Community 31 - "Community 31"
Cohesion: 0.37
Nodes (18): access_ok(), create(), CreateReq, drop(), exec(), ExecReq, list(), query() (+10 more)

### Community 32 - "Community 32"
Cohesion: 0.32
Nodes (17): audit_logs(), config_view(), get(), LimitsReq, notifications(), notifications_clear(), public(), ApiResult (+9 more)

### Community 33 - "Community 33"
Cohesion: 0.30
Nodes (14): check_owner(), create(), CreateReq, delete(), list(), revoke(), ApiResult, Json (+6 more)

### Community 34 - "Community 34"
Cohesion: 0.48
Nodes (14): data(), require_capability(), ApiResult, create(), delete(), get(), list(), ApiResult (+6 more)

### Community 35 - "Community 35"
Cohesion: 0.31
Nodes (13): checksum_file(), cleanup_old(), create(), delete(), download(), now_stamp(), restore(), Config (+5 more)

### Community 36 - "Community 36"
Cohesion: 0.28
Nodes (12): Q, ApiResult, Json, Option, Path, Query, State, String (+4 more)

### Community 37 - "Community 37"
Cohesion: 0.17
Nodes (12): blueprint_from_row(), Database, database_from_row(), list_databases(), list_websites(), Row, schedule_from_row(), server_from_row() (+4 more)

### Community 38 - "Community 38"
Cohesion: 0.36
Nodes (10): build_router(), main(), Arc, AtomicBool, Config, Result, Router, seed() (+2 more)

### Community 39 - "Community 39"
Cohesion: 0.36
Nodes (8): ServeDir, index(), IntoResponse, Response, State, spa_fallback(), static_dir(), Uri

### Community 40 - "Community 40"
Cohesion: 0.28
Nodes (5): human_bytes(), parse_duration(), Result, String, sanitize_name()

### Community 41 - "Community 41"
Cohesion: 0.38
Nodes (7): get_schedule(), list_schedule_tasks_conn(), list_schedules(), Connection, Schedule, ScheduleTask, update_schedule()

### Community 42 - "Community 42"
Cohesion: 0.60
Nodes (3): doctor(), require_root(), manage-node.sh script

### Community 43 - "Community 43"
Cohesion: 0.60
Nodes (3): doctor(), require_root(), manage-panel.sh script

### Community 44 - "Community 44"
Cohesion: 0.67
Nodes (3): fileIcon(), ic(), ICONS

## Knowledge Gaps
- **15 isolated node(s):** `common.sh script`, `VOLTPANEL_RAW`, `Limits`, `AtomicFlagGuard<'a>`, `OpenHow` (+10 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **1 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `AppState` connect `Community 30` to `Community 1`, `Community 4`, `Community 6`, `Community 11`, `Community 12`, `Community 16`, `Community 18`, `Community 19`, `Community 21`, `Community 22`, `Community 24`, `Community 26`, `Community 27`, `Community 28`, `Community 31`, `Community 32`, `Community 33`, `Community 34`, `Community 36`, `Community 38`, `Community 39`?**
  _High betweenness centrality (0.273) - this node is a cross-community bridge._
- **Why does `Db` connect `Community 11` to `Community 1`, `Community 3`, `Community 35`, `Community 37`, `Community 38`, `Community 4`, `Community 8`, `Community 41`, `Community 12`, `Community 14`, `Community 15`, `Community 17`, `Community 20`, `Community 23`, `Community 25`, `Community 30`?**
  _High betweenness centrality (0.271) - this node is a cross-community bridge._
- **Why does `ProcManager` connect `Community 4` to `Community 38`, `Community 11`, `Community 17`, `Community 22`, `Community 25`, `Community 30`?**
  _High betweenness centrality (0.052) - this node is a cross-community bridge._
- **What connects `common.sh script`, `VOLTPANEL_RAW`, `Limits` to the rest of the system?**
  _15 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Community 0` be split into smaller, more focused modules?**
  _Cohesion score 0.054140445509939066 - nodes in this community are weakly interconnected._
- **Should `Community 1` be split into smaller, more focused modules?**
  _Cohesion score 0.052850877192982454 - nodes in this community are weakly interconnected._
- **Should `Community 2` be split into smaller, more focused modules?**
  _Cohesion score 0.07467630231857875 - nodes in this community are weakly interconnected._