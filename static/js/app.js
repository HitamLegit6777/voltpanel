/* ============================================================
   VoltPanel SPA client v3 — SVG icons, i18n, full views
   ============================================================ */
"use strict";

const API = "/api";
const state = { user: null, page: "boot", server: null, servers: [], blueprints: [], pollers: [], consoleEs: null, consoleServer: null, lang: "en", charts: {}, sparks: {}, sparkKey: "", filePath: "/", fileId: null, pendingLogin: null, notifs: [], unread: 0, notifEs: null };

/* ---------- i18n ---------- */
const I18N = {
  en: {
    dashboard: "Dashboard", servers: "Servers", profile: "Profile", settings: "Settings",
    admin: "Admin", logout: "Logout", loading: "Loading…", none: "None",
    node: "Nodes", ram: "RAM", disk: "Disk", cpu: "CPU", load: "Load", uptime: "Uptime", processes: "Processes",
    status: "Status", name: "Name", owner: "Owner", actions: "Actions", size: "Size", type: "Type", modified: "Modified",
    start: "Start", restart: "Restart", stop: "Stop", kill: "Kill", save: "Save", cancel: "Cancel", create: "Create",
    delete: "Delete", edit: "Edit", download: "Download", upload: "Upload", copy: "Copy", rename: "Rename", archive: "Archive", extract: "Extract",
    console: "Console", files: "Files", databases: "Databases", backups: "Backups", schedules: "Schedules",
    sites: "Sites", domain: "Domain", enabled: "Enabled", ssl: "TLS",
    users: "Users", blueprints: "Eggs", system: "System", create_server: "New Server", create_user: "New User",
    suspend: "Suspend", unsuspend: "Unsuspend", reinstall: "Rebuild",
    metrics: "Metrics", allocations: "Network",
    login: "Sign in", signing_in: "Signing in…", verifying: "Verifying…", username: "Username", password: "Password", remember: "Remember me",
    welcome: "Linux-native workload control plane", no_servers: "No servers yet",
    auth_tagline: "Sandboxed Linux workloads, one control plane, zero container runtime.",
    auth_feat_1: "Bare-process isolation — no Docker daemon",
    auth_feat_2: "Per-workload CPU, RAM & bandwidth caps",
    auth_feat_3: "argon2id logins, TOTP + recovery codes",
    auth_signin: "Sign in", auth_signin_sub: "Enter your operator credentials.",
    auth_foot: "VoltPanel · encrypted sessions, no third-party auth",
    all_servers: "Servers", notifications: "Notifications", api_keys: "API Keys",
    notif_open: "Notifications", notif_empty: "No notifications yet", notif_all_read: "All caught up",
    notif_clear_all: "Clear all", notif_cleared: "Notifications cleared", notif_unread: "{n} unread",
    confirm_delete: "Delete?", confirm_restore: "Restore snapshot? Server will be stopped.",
    saved: "Saved", created: "Created", deleted: "Deleted", uploaded: "Uploaded",
    twofa: "Two-Factor Auth", enable_2fa: "Enable 2FA", verify: "Verify",
    retry: "Retry",
    console_follow: "Follow output", console_follow_paused: "Follow paused — click to resume",
    console_clear: "Clear console", console_download: "Download log",
    console_send: "Send", console_placeholder: "Type a command...",
    console_connecting: "connecting…", console_live: "live", console_disconnected: "disconnected",
    console_reconnecting: "reconnecting… ({n})", console_cleared: "Console cleared",
    console_truncated: "— history truncated, older lines dropped —",
    console_output_truncated: "— output truncated to the latest lines —",
    err_refresh_servers: "Could not refresh servers", err_load_files: "Could not load files",
    err_load_databases: "Could not load databases", err_load_backups: "Could not load backups",
    err_load_schedules: "Could not load schedules", err_load_sites: "Could not load sites",
    err_load_allocations: "Could not load allocations", err_load_keys: "Could not load API keys",
    err_load_webhooks: "Could not load webhooks", err_load_notifications: "Could not load notifications",
    err_load_servers: "Could not load servers", err_load_users: "Could not load users",
    err_load_blueprints: "Could not load blueprints", err_load_agents: "Could not load agents",
    err_load_flows: "Could not load flows", err_load_revisions: "Could not load revisions",
    err_load_drift: "Could not load drift",
    err_observatory: "System unavailable",
    obs_title: "Panel self-metrics", obs_since: "since", obs_requests: "Requests",
    obs_ok: "ok", obs_errors: "errors", obs_rate: "req / min", obs_last: "last",
    obs_minutes: "min", obs_pool: "DB pool", obs_idle: "idle",
    obs_uptime: "Panel uptime", obs_sched: "Scheduler pending",
    obs_webhooks: "Webhook pending", obs_pending: "pending", obs_mirror: "mirror",
    obs_last_tick: "last tick", obs_pool_saturated: "pool saturated",
    err_write_blocked: "Write blocked", err_query_failed: "Query failed",
    runway_title: "Launch runway", runway_sub: "Three steps to your first live workload",
    runway_workspace: "Launch a server", runway_workspace_desc: "Pick a blueprint and compose it",
    runway_node: "Attach an execution agent", runway_node_desc: "Extend the execution fabric beyond this machine",
    runway_backup: "Take the first snapshot", runway_backup_desc: "Lock in a verified backup",
    runway_compose: "Compose", runway_attach: "Attach agent", runway_ask_admin: "Ask an administrator",
    runway_waiting: "Waits for a server", runway_progress: "Launch progress", runway_dismiss: "Dismiss",
    runway_port_hint: "reserved on the agent", runway_start_on_create: "Launch immediately after create",
    runway_no_vars: "This blueprint declares no launch inputs.", runway_no_launch: "This blueprint has no launch command.",
    runway_need_blueprint: "No blueprints yet — create one in Eggs first.", runway_preview: "Live launch preview",
    runway_launch_cmd: "Launch command", runway_resources: "Resources", runway_endpoint: "Endpoint", runway_blueprint: "Blueprint", close: "Close",
    place_label: "Placement", place_auto: "Auto — best-fit agent",
    place_hint_auto: "VoltPanel scores every eligible agent by free headroom and pins the winner.",
    place_node_online: "online", place_node_offline: "offline", place_need_node: "Attach an execution agent first",
    place_free: "{mem} free · {disk} disk", place_tags: "Required tags", place_tags_ph: "gpu, eu-west (comma-separated)",
    place_location: "Location", place_location_ph: "any", place_target: "Target agent",
    place_no_room: "{node} is short on capacity for this size — launch may be refused.",
    place_auto_pool: "Auto picks from {n} eligible agent(s).", place_auto_none: "No agent matches those tags/location yet.",
    registry: "Registry", reg_library: "Library", reg_search_ph: "Search packages…",
    reg_install: "Install", reg_publish: "Publish", reg_published: "Published", reg_installed: "Installed",
    reg_signed: "Signed", reg_unsigned: "Unsigned", reg_bad_sig: "Signature invalid",
    reg_no_packages: "No packages published yet", reg_ver_label: "Version of {name}", reg_empty_hint: "Publish a blueprint from the Library tab, or install by URL through the API.",
    reg_install_confirm: "Install {name} v{version} into the local blueprint store?",
    reg_publish_confirm: "Publish the latest revision of {name} to the registry?",
    reg_published_ok: "Published to registry", reg_imported: "Package installed",
    reg_unsigned_warn: "Package is unsigned — installed without signature verification",
    reg_publish_unsigned_warn: "No signing key configured — package published unsigned",
    reg_versions: "versions", reg_signing_key: "Signing key", reg_gen_key: "Generate key",
    reg_key_generated: "Signing key generated — fingerprint {fp}",
    reg_signing_off: "Signing disabled", reg_clear_key: "Disable signing", reg_key_cleared: "Signing disabled",
    squads: "Squads", squad_new: "New squad", squad_name: "Squad name", squad_name_ph: "e.g. Platform Engineering",
    squad_members: "Members", squad_servers: "Servers", squad_role: "Role", squad_add_member: "Add member",
    squad_user_ph: "Select user…", squad_no_squads: "No squads yet — create one to group members and servers.",
    squad_no_members: "No members yet", squad_server_assignment: "Server assignment", squad_save_servers: "Save servers",
    squad_created: "Squad created", squad_saved: "Squad saved", squad_deleted: "Squad deleted",
    squad_delete_confirm: "Delete squad \"{name}\"? Memberships are revoked; assigned servers are not touched.",
    squad_servers_saved: "Server assignment saved", member_added: "Member added", member_removed: "Member removed",
    member_remove_confirm: "Remove {name} from this squad?", member_role_updated: "Role updated",
    squad_memberships: "Squad memberships", squad_no_memberships: "Not a member of any squad",
    squad_my_role: "You are {role}",
    user_detail: "User detail", back: "Back", control_center: "Admin", control_center_only: "Admin is for administrators only",
    obs_endpoints: "Endpoints", obs_host_resources: "Host resources",
    role_viewer: "Viewer", role_operator: "Operator", role_developer: "Developer", role_manager: "Manager",
    err_load_squads: "Could not load squads", err_load_squad: "Could not load squad", err_load_user_detail: "Could not load user detail",
    mirror_status_ok: "mirror ok", mirror_status_ok_title: "Offsite mirror healthy",
    mirror_status_degraded: "mirror degraded", mirror_status_degraded_title: "Mirror sync failing — re-sync or check the mirror path",
    mirror_status_disabled: "mirror off", mirror_status_disabled_title: "Offsite mirror is disabled",
    mirror_sync: "Sync mirror", mirror_sync_confirm: "Re-sync the offsite mirror now? Missing archives are copied from the primary store and mirror retention is enforced.",
    mirror_sync_running: "Syncing…", mirror_sync_done: "Mirror synced — {copied} copied, {removed} removed · {status}",
    gate: "Gate", gate_none: "none", gate_exit: "exit", gate_signal: "signal",
    gate_after_task: "after task {n} exits {code}", gate_wait_signal: "wait {event} ≤{s}s",
    gate_unknown: "unknown gate: {raw}", gate_task_opt: "task {n}", gate_no_earlier: "no earlier task",
    gate_previous_task: "Previous task", gate_expected_code: "Expected exit code",
    gate_event: "Event", gate_timeout: "Timeout (s)", gate_custom: "custom…",
    gate_custom_ph: "event name, e.g. deploy.done", gate_lock: "server locked",
    gate_lock_title: "Signal gates wait for webhook events of this server",
    gate_err_index: "exit gate must reference an earlier task",
    gate_err_code: "exit code must be an integer",
    gate_err_event: "signal event is required",
    gate_err_timeout: "timeout must be an integer 1–3600",
    gate_warn_unknown: "Preserving unrecognized gate — {raw}",
    palette_search: "Search commands and pages…", palette_scoped_hint: "Tip: s: servers · u: users · bp: blueprints · ev: events",
    palette_servers: "Servers", palette_users: "Users", palette_blueprints: "Eggs", palette_events: "Events",
    palette_commands: "Commands", palette_create: "Create", palette_theme: "Toggle theme",
    palette_no_results: "No matches", palette_loading: "Loading…",
    palette_new_workspace: "New server", palette_new_user: "New user", palette_new_squad: "New squad",
    palette_new_blueprint: "New blueprint", palette_attach_node: "Attach agent",
    sel_all: "Select all", sel_n: "{n} selected", sel_clear: "Clear selection",
    bulk_start: "Start", bulk_stop: "Stop", bulk_restart: "Restart", bulk_suspend: "Suspend", bulk_unsuspend: "Unsuspend",
    bulk_confirm_stop: "Stop {n} selected server(s)? Running processes will be terminated.",
    bulk_done: "Bulk action complete — {ok} ok, {fail} failed",
    node_detail: "Agent detail", node_identity: "Identity", node_heartbeat: "Heartbeat", node_never_hb: "never",
    node_agent_version: "Agent version", node_host: "Hostname", node_os_arch: "OS / arch",
    node_last_error: "Last error", node_tls_fp: "TLS fingerprint",
    node_edit: "Edit agent", node_edit_hint: "Updates are applied atomically by the panel.",
    node_new_title: "Attach an execution agent", node_new_sub: "Register a machine, then run one command on it to enroll.",
    node_name_ph: "agent-eu-1", node_url_hint: "Where the panel reaches the agent (host:port).",
    node_location_ph: "id-jakarta", node_location_hint: "Free-form region label used for auto-placement.",
    node_fp: "Pinned TLS fingerprint", node_fp_create_ph: "64-hex SHA-256 (optional)",
    node_fp_hint: "If set, enrollment is refused unless the agent presents this exact certificate.",
    node_enroll_title: "One command to finish", node_enroll_sub: "Run this on the agent machine — the token is single-use and expires.",
    node_copy_cmd: "Copy command", node_name_req: "Name needs at least 2 characters.", node_url_req: "Enter the agent endpoint URL.",
    node_fp_bad: "Fingerprint must be 64 hex characters.",
    node_url: "Endpoint", node_location: "Location", node_tags: "Tags", node_tags_ph: "comma, separated, tags",
    node_limits: "Limits", node_mem_limit: "Memory limit (MB)", node_disk_limit: "Disk limit (MB)",
    node_mem_over: "Memory overallocation (%)", node_disk_over: "Disk overallocation (%)",
    node_sched: "Scheduling", node_enabled: "Accept enrollments", node_schedulable: "Schedulable", node_maintenance: "Maintenance mode",
    node_expected_fp: "Expected fingerprint", node_fp_ph: "64-hex SHA-256 (empty clears)", node_saved: "Agent updated",
    node_drain: "Drain", node_drain_active: "draining ({mode})", node_drain_hold: "Hold", node_drain_stop: "Hold + stop",
    node_drain_hold_hint: "Cordon: no new placements, running workloads stay up.",
    node_drain_stop_hint: "Cordon and stop running workloads.",
    node_drain_reason: "Reason", node_drain_reason_ph: "e.g. rack maintenance",
    node_drain_deadline: "Deadline (hours)", node_drain_deadline_ph: "empty = no auto-lift",
    node_drain_started: "Drain set", node_drain_cleared: "Drain lifted", node_drain_clear: "Lift drain",
    node_drain_failed: "Stop failures", node_drain_clear_confirm: "Lift the drain and restore scheduling?",
    node_events: "Events", node_no_events: "No agent events yet", node_updated: "updated",
    skip_link: "Skip to content", sort_asc: "Sorted ascending", sort_desc: "Sorted descending", sort_none: "Click to sort",
    es_db_t: "No databases yet", es_db_h: "Spin up a SQLite database this server can read and write through Databases.",
    es_bk_t: "No snapshots yet", es_bk_h: "Capture a verified archive of this server you can restore or download later.",
    es_sch_t: "No schedules yet", es_sch_h: "Automate power, commands, backups or alerts on a cron cadence.",
    es_site_t: "No sites yet", es_site_h: "Route a domain to this server as a static host or reverse proxy.",
    es_alloc_t: "No endpoints yet", es_alloc_h: "Reserve a port; the first one becomes this server's primary endpoint.",
    es_key_t: "No access tokens", es_key_h: "Create a scoped token to reach the API without your session cookie.",
    es_wh_t: "No webhooks", es_wh_h: "Get signed HTTP callbacks when server and fleet events fire.",
    es_notif_t: "No notifications", es_notif_h: "Notifications and audit-worthy events will surface here.",
    es_runs_t: "No runs yet", es_runs_h: "This schedule hasn't fired — trigger it or wait for its next window.",
    es_deliv_t: "No deliveries yet", es_deliv_h: "Delivery attempts appear once a matching event fires.",
    es_files_none_t: "This folder is empty", es_search_none_t: "No matches",
    cut: "Cut", clear: "Clear", paste_here: "Paste here", add_task: "Add task", save_changes: "Save changes",
    new_webhook: "New webhook", webhooks: "Webhooks", new_blueprint: "New blueprint",
    flows_eyebrow: "EVENT-DRIVEN CONTROL", flows_title: "Schedule runway", flows_sub: "Coordinate lifecycle commands, snapshots, and tasks across every server.",
    bp_eyebrow: "VOLT SPECIFICATION", bp_title: "Eggs", bp_sub: "Compose portable launch plans, then publish and install them through the registry.",
    fabric_eyebrow: "EXECUTION FABRIC", fabric_title: "Nodes", fabric_sub: "Capacity, isolation, placement, and health across every execution host.",
    es_nodes_t: "No agents attached", es_nodes_h: "Local isolated execution keeps working. Attach an agent to spread workloads across machines.",
    a_start: "Start server", a_restart: "Restart server", a_stop: "Stop server", a_kill: "Force kill server",
    a_refresh: "Refresh", a_new_file: "New file", a_new_folder: "New folder", a_pull_url: "Pull from URL",
    a_rename: "Rename", a_more: "More actions", a_run_now: "Run now", a_run_history: "Run history",
    a_test_node: "Test agent", a_reenroll: "Re-enroll", a_delete_node: "Delete agent",
    a_toggle_wh: "Toggle webhook", a_test_ping: "Send test ping", a_deliveries: "Deliveries",
    a_open_palette: "Open command palette", a_open_nav: "Open navigation", a_make_primary: "Make primary",
    a_copy_endpoint: "Copy endpoint", a_edit_notes: "Edit notes", a_detach: "Detach", a_verify: "Verify checksum",
    edit_schedule: "Edit schedule", new_schedule: "New schedule", pulling_file: "Pulling file",
    bp_new_sub: "Define how this server starts.", search: "Search",
    protected: "Protected", isolation_on: "Server isolation enabled", filter_files: "Filter files", filter_ph: "Filter…", isolation_ns: "Namespace + cgroup isolation",
    bp_category: "Category", bp_category_hint: "For grouping in the catalog.", bp_runtime: "Runtime",
    bp_launch: "Launch command", bp_launch_hint: "The command executed inside the isolated server directory.",
    bp_name_hint: "Shown when creating a server.", bp_create: "Create blueprint", bp_copied: "VoltSpec copied to clipboard",
    sch_name_ph: "daily restart", sch_cron_label: "Cron", sch_tasks: "Tasks", sch_backoff: "backoff s", retries: "retries",
    sch_cmd_ph: "command to send", sch_backup_ph: "backup name (optional)", sch_payload_ph: "payload (optional)", sch_payload_label: "Task payload",
    bk_name_opt: "Name", bk_optional: "optional", bk_ignore: "Ignore patterns", bk_ignore_hint: ".gitignore-style globs, one per line",
    bk_ignore_note: "Excluded from the archive. Local servers only — remote agents reject ignore patterns.", bk_create_title: "New snapshot",
    copied: "Copied", moved: "Moved", extracted: "Extracted", downloaded: "Downloaded",
    t_perms_updated: "Permissions updated", t_crash_saved: "Crash policy saved", t_crash_reset: "Crash burst reset",
    t_upload_failed: "Upload failed", t_install_queued: "Install queued", t_restored: "Restored — server refreshed",
    t_transfer_cancelled: "Transfer cancelled", t_cancelling: "Cancelling…", t_triggered: "Triggered",
    t_primary_updated: "Primary endpoint updated", t_detached: "Detached", t_pass_changed: "Password changed",
    t_key_revoked: "Token revoked", t_wh_updated: "Webhook updated", t_bp_created: "Blueprint created",
    e_sch_not_found: "Schedule not found", e_name_cron: "Name and cron are required", e_one_task: "Add at least one task",
    e_domain_req: "Domain is required", e_valid_port: "Enter a valid port (1–65535)", e_pick_cap: "Select at least one capability (or Full access)", e_required_name: "Name is required", e_required_url: "URL is required", e_pick_user: "Select a user to add",
    e_node_short: "Agent name too short", confirm_revoke_key: "Revoke this token? It stops working immediately but stays listed for audit.",
    recovery_codes: "Recovery codes", recovery_codes_hint: "Save these one-time codes somewhere safe. Each one works exactly once — if you lose your authenticator app, a recovery code is your only way back in.",
    recovery_saved: "Codes saved", recovery_confirm: "I saved the codes",
    regenerate: "Regenerate", disable_2fa: "Disable 2FA", reset_2fa: "Reset 2FA",
    reset_2fa_confirm: "Reset 2FA for {name}? Their secret and recovery codes will be cleared and they must re-enroll.",
    rec_reauth: "Re-authenticate", rec_reauth_sub: "Enter your current password and a code from your authenticator app to rotate the recovery codes.",
    recovery_use: "Use a recovery code", recovery_ph: "XXXXX-XXXXX",
    totp_code: "6-digit code", disable_2fa_body: "Enter a code from your authenticator app to turn off two-factor auth. Your recovery codes are deleted too.",
    twofa_sub: "Enter the code from your authenticator app.", use_authenticator: "Use authenticator app",
    twofa_scan: "Scan with any authenticator app (Google Authenticator, Authy, 1Password).", twofa_secret: "Secret",
    token_created: "Access token created", token_once: "Shown once — copy it now, it can't be retrieved later.", copy_token: "Copy token", done: "Done",
    wh_secret_title: "Webhook signing secret", wh_secret_once: "Shown once — copy it now, it can't be retrieved later.", copy_secret: "Copy secret",
    qa_compose: "Create server", qa_compose_sub: "Launch from a blueprint", qa_attach: "Add node", qa_attach_sub: "Extend the execution fabric",
    qa_api: "API access", qa_api_sub: "Manage scoped credentials", qa_security: "Account security", qa_security_sub: "Password and two-factor auth", recent_activity: "Recent activity",
    quick_actions: "Quick actions", webhook_created: "Webhook created",
    watchers: "Watchers", pause: "Pause", enable: "Enable",
    watchers_badge: "reactive", watchers_hint: "log-pattern rules that fire an action on match",
    watchers_empty: "No watchers yet — add a rule to react to console output",
    watcher_fired: "fired {n}× · {ago}", watcher_never: "never fired",
    watcher_new: "New watcher", watcher_edit: "Edit watcher",
    watcher_name_ph: "e.g. Crash guard", watcher_pattern: "Match pattern",
    watcher_pattern_ph: "e.g. Out of memory", watcher_regex: "Treat pattern as regular expression",
    watcher_action: "On match", watcher_cooldown: "Cooldown", watcher_cooldown_hint: "seconds to wait before firing again",
    watcher_level: "Notification level", watcher_command: "Command to send", watcher_command_ph: "e.g. say Server is restarting",
    watcher_no_payload: "No extra input — this action runs as-is.",
    watcher_confirm_del: "Delete watcher \"{name}\"?",
    wt_notify: "Notify", wt_command: "Run command", wt_restart: "Restart", wt_stop: "Stop",
    lvl_info: "Info", lvl_warn: "Warning", lvl_error: "Error",
    e_watcher_fields: "Name and pattern are required", e_watcher_command: "Command action needs a command to send",
  },
  id: {
    dashboard: "Dashboard", servers: "Server", profile: "Profil", settings: "Pengaturan",
    admin: "Admin", logout: "Keluar", loading: "Memuat…", none: "Tidak ada",
    node: "Node", ram: "RAM", disk: "Disk", cpu: "CPU", load: "Beban", uptime: "Aktif", processes: "Proses",
    status: "Status", name: "Nama", owner: "Pemilik", actions: "Aksi", size: "Ukuran", type: "Tipe", modified: "Diubah",
    start: "Mulai", restart: "Ulang", stop: "Hentikan", kill: "Paksa", save: "Simpan", cancel: "Batal", create: "Buat",
    delete: "Hapus", edit: "Ubah", download: "Unduh", upload: "Unggah", copy: "Salin", rename: "Ganti nama", archive: "Arsip", extract: "Ekstrak",
    console: "Konsol", files: "File", databases: "Database", backups: "Backup", schedules: "Jadwal",
    sites: "Situs", domain: "Domain", enabled: "Aktif", ssl: "TLS",
    users: "Pengguna", blueprints: "Eggs", system: "Sistem", create_server: "Server Baru", create_user: "Pengguna Baru",
    suspend: "Nonaktifkan", unsuspend: "Aktifkan", reinstall: "Bangun Ulang",
    metrics: "Metrik", allocations: "Jaringan",
    notif_open: "Notifikasi", notif_empty: "Belum ada notifikasi", notif_all_read: "Semua sudah dibaca",
    notif_clear_all: "Bersihkan semua", notif_cleared: "Notifikasi dibersihkan", notif_unread: "{n} belum dibaca",
    welcome: "Control plane workload Linux-native", no_servers: "Belum ada server",
    auth_tagline: "Workload Linux ter-sandbox, satu control plane, tanpa runtime kontainer.",
    auth_feat_1: "Isolasi bare-process — tanpa daemon Docker",
    auth_feat_2: "Batas CPU, RAM & bandwidth per workload",
    auth_feat_3: "Login argon2id, TOTP + kode pemulihan",
    auth_signin: "Masuk", auth_signin_sub: "Masukkan kredensial operator Anda.",
    auth_foot: "VoltPanel · sesi terenkripsi, tanpa auth pihak ketiga",
    all_servers: "Server", notifications: "Notifikasi", api_keys: "Kunci API",
    confirm_delete: "Hapus?", confirm_restore: "Pulihkan snapshot? Server akan dihentikan.",
    saved: "Tersimpan", created: "Dibuat", deleted: "Dihapus", uploaded: "Terunggah",
    twofa: "Autentikasi Dua Faktor", enable_2fa: "Aktifkan 2FA", verify: "Verifikasi",
    retry: "Muat ulang",
    console_follow: "Ikuti output", console_follow_paused: "Jeda — klik untuk melanjutkan",
    console_clear: "Bersihkan konsol", console_download: "Unduh log",
    console_send: "Kirim", console_placeholder: "Ketik perintah...",
    console_connecting: "Menghubungkan…", console_live: "Langsung", console_disconnected: "Terputus",
    console_reconnecting: "Menyambung ulang… ({n})", console_cleared: "Konsol dibersihkan",
    console_truncated: "— riwayat dipangkas, baris lama dihapus —",
    console_output_truncated: "— output dipangkas ke baris terbaru —",
    err_refresh_servers: "Tidak dapat memuat ulang server", err_load_files: "Tidak dapat memuat file",
    err_load_databases: "Tidak dapat memuat database", err_load_backups: "Tidak dapat memuat backup",
    err_load_schedules: "Tidak dapat memuat jadwal", err_load_sites: "Tidak dapat memuat situs",
    err_load_allocations: "Tidak dapat memuat alokasi", err_load_keys: "Tidak dapat memuat token API",
    err_load_webhooks: "Tidak dapat memuat webhook", err_load_notifications: "Tidak dapat memuat notifikasi",
    err_load_servers: "Tidak dapat memuat server", err_load_users: "Tidak dapat memuat pengguna",
    err_load_blueprints: "Tidak dapat memuat blueprint", err_load_agents: "Tidak dapat memuat agen",
    err_load_flows: "Tidak dapat memuat flow", err_load_revisions: "Tidak dapat memuat revisi",
    obs_title: "Metrik panel", obs_since: "sejak", obs_requests: "Permintaan",
    obs_ok: "ok", obs_errors: "galat", obs_rate: "req / mnt", obs_last: "terakhir",
    obs_minutes: "mnt", obs_pool: "Pool DB", obs_idle: "idle",
    obs_uptime: "Aktif panel", obs_sched: "Antrean jadwal",
    obs_webhooks: "Antrean webhook", obs_pending: "antre", obs_mirror: "mirror",
    obs_last_tick: "tik terakhir", obs_pool_saturated: "pool penuh",
    err_load_drift: "Tidak dapat memuat drift",
    err_observatory: "Observatorium tidak tersedia",
    err_write_blocked: "Penulisan diblokir", err_query_failed: "Kueri gagal",
    runway_title: "Landasan peluncuran", runway_sub: "Tiga langkah menuju workload live pertama Anda",
    runway_workspace: "Luncurkan server", runway_workspace_desc: "Pilih blueprint dan susun",
    runway_node: "Pasang agen eksekusi", runway_node_desc: "Perluas fabric eksekusi melampaui mesin ini",
    runway_backup: "Ambil snapshot pertama", runway_backup_desc: "Kunci backup terverifikasi",
    runway_compose: "Susun", runway_attach: "Pasang agen", runway_ask_admin: "Minta administrator",
    runway_waiting: "Menunggu server", runway_progress: "Progres peluncuran", runway_dismiss: "Tutup",
    runway_port_hint: "dipesan di agen", runway_start_on_create: "Luncurkan segera setelah dibuat",
    runway_no_vars: "Blueprint ini tidak mendeklarasikan input peluncuran.", runway_no_launch: "Blueprint ini tidak memiliki perintah peluncuran.",
    runway_need_blueprint: "Belum ada blueprint — buat dulu di Eggs.", runway_preview: "Pratinjau peluncuran langsung",
    runway_launch_cmd: "Perintah peluncuran", runway_resources: "Sumber daya", runway_endpoint: "Endpoint", runway_blueprint: "Blueprint", close: "Tutup",
    place_label: "Penempatan", place_auto: "Otomatis — agen paling pas",
    place_hint_auto: "VoltPanel menilai tiap agen yang memenuhi syarat berdasarkan sisa kapasitas lalu mengunci pemenangnya.",
    place_node_online: "online", place_node_offline: "offline", place_need_node: "Pasang agen eksekusi terlebih dahulu",
    place_free: "{mem} bebas · {disk} disk", place_tags: "Tag wajib", place_tags_ph: "gpu, eu-west (pisahkan koma)",
    place_location: "Lokasi", place_location_ph: "bebas", place_target: "Agen tujuan",
    place_no_room: "{node} kekurangan kapasitas untuk ukuran ini — peluncuran bisa ditolak.",
    place_auto_pool: "Otomatis memilih dari {n} agen yang memenuhi syarat.", place_auto_none: "Belum ada agen yang cocok dengan tag/lokasi itu.",
    registry: "Registri", reg_library: "Pustaka", reg_search_ph: "Cari paket…",
    reg_install: "Pasang", reg_publish: "Terbitkan", reg_published: "Terbit", reg_installed: "Terpasang",
    reg_signed: "Ditandatangani", reg_unsigned: "Tanpa tanda tangan", reg_bad_sig: "Tanda tangan tidak valid",
    reg_no_packages: "Belum ada paket diterbitkan", reg_ver_label: "Versi {name}", reg_empty_hint: "Terbitkan blueprint dari tab Pustaka, atau pasang melalui URL lewat API.",
    reg_install_confirm: "Pasang {name} v{version} ke penyimpanan blueprint lokal?",
    reg_publish_confirm: "Terbitkan revisi terbaru {name} ke registri?",
    reg_published_ok: "Diterbitkan ke registri", reg_imported: "Paket terpasang",
    reg_unsigned_warn: "Paket tanpa tanda tangan — dipasang tanpa verifikasi tanda tangan",
    reg_publish_unsigned_warn: "Tidak ada kunci penandatangan — paket diterbitkan tanpa tanda tangan",
    reg_versions: "versi", reg_signing_key: "Kunci penandatangan", reg_gen_key: "Buat kunci",
    reg_key_generated: "Kunci penandatangan dibuat — sidik jari {fp}",
    reg_signing_off: "Penandatanganan nonaktif", reg_clear_key: "Nonaktifkan penandatangan", reg_key_cleared: "Penandatanganan dinonaktifkan",
    squads: "Squad", squad_new: "Squad baru", squad_name: "Nama squad", squad_name_ph: "mis. Platform Engineering",
    squad_members: "Anggota", squad_servers: "Server", squad_role: "Peran", squad_add_member: "Tambah anggota",
    squad_user_ph: "Pilih pengguna…", squad_no_squads: "Belum ada squad — buat satu untuk mengelompokkan anggota dan server.",
    squad_no_members: "Belum ada anggota", squad_server_assignment: "Penugasan server", squad_save_servers: "Simpan server",
    squad_created: "Squad dibuat", squad_saved: "Squad disimpan", squad_deleted: "Squad dihapus",
    squad_delete_confirm: "Hapus squad \"{name}\"? Keanggotaan dicabut; server yang ditugaskan tidak terpengaruh.",
    squad_servers_saved: "Penugasan server disimpan", member_added: "Anggota ditambahkan", member_removed: "Anggota dihapus",
    member_remove_confirm: "Hapus {name} dari squad ini?", member_role_updated: "Peran diperbarui",
    squad_memberships: "Keanggotaan squad", squad_no_memberships: "Bukan anggota squad mana pun",
    squad_my_role: "Anda adalah {role}",
    user_detail: "Detail pengguna", back: "Kembali", control_center: "Admin", control_center_only: "Admin hanya untuk administrator",
    obs_endpoints: "Endpoint", obs_host_resources: "Sumber daya host",
    role_viewer: "Penonton", role_operator: "Operator", role_developer: "Pengembang", role_manager: "Manajer",
    err_load_squads: "Tidak dapat memuat squad", err_load_squad: "Tidak dapat memuat squad", err_load_user_detail: "Tidak dapat memuat detail pengguna",
    mirror_status_ok: "mirror ok", mirror_status_ok_title: "Mirror jarak jauh sehat",
    mirror_status_degraded: "mirror menurun", mirror_status_degraded_title: "Sinkron mirror gagal — sinkronkan ulang atau periksa jalur mirror",
    mirror_status_disabled: "mirror nonaktif", mirror_status_disabled_title: "Mirror jarak jauh nonaktif",
    mirror_sync: "Sinkronkan mirror", mirror_sync_confirm: "Sinkronkan ulang mirror jarak jauh sekarang? Arsip yang hilang disalin dari penyimpanan utama dan retensi mirror diberlakukan.",
    mirror_sync_running: "Menyinkronkan…", mirror_sync_done: "Mirror disinkronkan — {copied} disalin, {removed} dihapus · {status}",
    gate: "Gerbang", gate_none: "tanpa", gate_exit: "exit", gate_signal: "sinyal",
    gate_after_task: "setelah tugas {n} keluar {code}", gate_wait_signal: "tunggu {event} ≤{s}dtk",
    gate_unknown: "gerbang tak dikenal: {raw}", gate_task_opt: "tugas {n}", gate_no_earlier: "tidak ada tugas sebelumnya",
    gate_previous_task: "Tugas sebelumnya", gate_expected_code: "Kode keluar yang diharapkan",
    gate_event: "Peristiwa", gate_timeout: "Batas waktu (dtk)", gate_custom: "kustom…",
    gate_custom_ph: "nama peristiwa, mis. deploy.done", gate_lock: "server dikunci",
    gate_lock_title: "Gerbang sinyal menunggu peristiwa webhook server ini",
    gate_err_index: "gerbang exit harus merujuk tugas sebelumnya",
    gate_err_code: "kode keluar harus bilangan bulat",
    gate_err_event: "peristiwa sinyal wajib diisi",
    gate_err_timeout: "batas waktu harus bilangan bulat 1–3600",
    gate_warn_unknown: "Mempertahankan gerbang tak dikenal — {raw}",
    login: "Masuk", signing_in: "Sedang masuk…", verifying: "Memverifikasi…", username: "Nama pengguna", password: "Kata sandi", remember: "Ingat saya",
    palette_search: "Cari perintah dan halaman…", palette_scoped_hint: "Tips: s: server · u: pengguna · bp: blueprint · ev: peristiwa",
    palette_servers: "Server", palette_users: "Pengguna", palette_blueprints: "Eggs", palette_events: "Peristiwa",
    palette_commands: "Perintah", palette_create: "Buat", palette_theme: "Ganti tema",
    palette_no_results: "Tidak ada hasil", palette_loading: "Memuat…",
    palette_new_workspace: "Server baru", palette_new_user: "Pengguna baru", palette_new_squad: "Squad baru",
    palette_new_blueprint: "Blueprint baru", palette_attach_node: "Lampirkan agen",
    sel_all: "Pilih semua", sel_n: "{n} dipilih", sel_clear: "Bersihkan pilihan",
    bulk_start: "Mulai", bulk_stop: "Hentikan", bulk_restart: "Ulang", bulk_suspend: "Nonaktifkan", bulk_unsuspend: "Aktifkan",
    bulk_confirm_stop: "Hentikan {n} server terpilih? Proses yang berjalan akan diakhiri.",
    bulk_done: "Aksi massal selesai — {ok} berhasil, {fail} gagal",
    node_detail: "Detail agen", node_identity: "Identitas", node_heartbeat: "Detak jantung", node_never_hb: "tidak pernah",
    node_agent_version: "Versi agen", node_host: "Nama host", node_os_arch: "OS / arsitektur",
    node_last_error: "Kesalahan terakhir", node_tls_fp: "Sidik jari TLS",
    node_edit: "Ubah agen", node_edit_hint: "Pembaruan diterapkan secara atomik oleh panel.",
    node_new_title: "Pasang agen eksekusi", node_new_sub: "Daftarkan sebuah mesin, lalu jalankan satu perintah di mesin itu untuk mendaftar.",
    node_name_ph: "agent-eu-1", node_url_hint: "Alamat panel menjangkau agen (host:port).",
    node_location_ph: "id-jakarta", node_location_hint: "Label wilayah bebas untuk penempatan otomatis.",
    node_fp: "Sidik jari TLS terpinned", node_fp_create_ph: "64-hex SHA-256 (opsional)",
    node_fp_hint: "Jika diisi, pendaftaran ditolak kecuali agen menunjukkan sertifikat persis ini.",
    node_enroll_title: "Satu perintah untuk menyelesaikan", node_enroll_sub: "Jalankan ini di mesin agen — token sekali pakai dan akan kedaluwarsa.",
    node_copy_cmd: "Salin perintah", node_name_req: "Nama minimal 2 karakter.", node_url_req: "Masukkan URL endpoint agen.",
    node_fp_bad: "Sidik jari harus 64 karakter heksadesimal.",
    node_url: "Endpoint", node_location: "Lokasi", node_tags: "Tag", node_tags_ph: "dipisah, koma, tag",
    node_limits: "Batas", node_mem_limit: "Batas memori (MB)", node_disk_limit: "Batas disk (MB)",
    node_mem_over: "Overalokasi memori (%)", node_disk_over: "Overalokasi disk (%)",
    node_sched: "Penjadwalan", node_enabled: "Terima pendaftaran", node_schedulable: "Dapat dijadwalkan", node_maintenance: "Mode perawatan",
    node_expected_fp: "Sidik jari yang diharapkan", node_fp_ph: "SHA-256 64-heksadesimal (kosong untuk menghapus)", node_saved: "Agen diperbarui",
    node_drain: "Kuras", node_drain_active: "menguras ({mode})", node_drain_hold: "Tahan", node_drain_stop: "Tahan + hentikan",
    node_drain_hold_hint: "Kordon: tanpa penempatan baru, workload berjalan tetap aktif.",
    node_drain_stop_hint: "Kordon dan hentikan workload yang berjalan.",
    node_drain_reason: "Alasan", node_drain_reason_ph: "mis. perawatan rak",
    node_drain_deadline: "Batas waktu (jam)", node_drain_deadline_ph: "kosong = tanpa angkat-otomatis",
    node_drain_started: "Kuras ditetapkan", node_drain_cleared: "Kuras diangkat", node_drain_clear: "Angkat kuras",
    node_drain_failed: "Gagal menghentikan", node_drain_clear_confirm: "Angkat kuras dan pulihkan penjadwalan?",
    node_events: "Peristiwa", node_no_events: "Belum ada peristiwa agen", node_updated: "diperbarui",
    skip_link: "Lewati ke konten", sort_asc: "Diurutkan menaik", sort_desc: "Diurutkan menurun", sort_none: "Klik untuk mengurutkan",
    es_db_t: "Belum ada database", es_db_h: "Buat database SQLite yang bisa dibaca dan ditulis server ini lewat Database.",
    es_bk_t: "Belum ada snapshot", es_bk_h: "Ambil arsip terverifikasi dari server ini untuk dipulihkan atau diunduh nanti.",
    es_sch_t: "Belum ada jadwal", es_sch_h: "Otomatiskan daya, perintah, backup, atau peringatan dengan irama cron.",
    es_site_t: "Belum ada situs", es_site_h: "Arahkan domain ke server ini sebagai host statis atau reverse proxy.",
    es_alloc_t: "Belum ada endpoint", es_alloc_h: "Pesan sebuah port; yang pertama menjadi endpoint utama server ini.",
    es_key_t: "Belum ada token akses", es_key_h: "Buat token ber-scope untuk mengakses API tanpa cookie sesi Anda.",
    es_wh_t: "Belum ada webhook", es_wh_h: "Terima callback HTTP bertanda tangan saat peristiwa server dan fleet terjadi.",
    es_notif_t: "Belum ada notifikasi", es_notif_h: "Peringatan fleet dan peristiwa penting audit akan muncul di sini.",
    es_runs_t: "Belum ada eksekusi", es_runs_h: "Jadwal ini belum berjalan — picu sekarang atau tunggu jendela berikutnya.",
    es_deliv_t: "Belum ada pengiriman", es_deliv_h: "Upaya pengiriman muncul setelah ada peristiwa yang cocok.",
    es_files_none_t: "Folder ini kosong", es_search_none_t: "Tidak ada hasil",
    cut: "Potong", clear: "Bersihkan", paste_here: "Tempel di sini", add_task: "Tambah tugas", save_changes: "Simpan perubahan",
    new_webhook: "Webhook baru", webhooks: "Webhook", new_blueprint: "Blueprint baru",
    flows_eyebrow: "KONTROL BERBASIS PERISTIWA", flows_title: "Landasan Jadwal", flows_sub: "Koordinasikan perintah lifecycle, snapshot, dan tugas di seluruh server.",
    bp_eyebrow: "SPESIFIKASI VOLT", bp_title: "Eggs", bp_sub: "Susun rencana peluncuran portabel, lalu terbitkan dan pasang lewat registri.",
    fabric_eyebrow: "FABRIC EKSEKUSI", fabric_title: "Node", fabric_sub: "Kapasitas, isolasi, penempatan, dan kesehatan di setiap host eksekusi.",
    es_nodes_t: "Belum ada agen terpasang", es_nodes_h: "Eksekusi lokal ter-isolasi tetap berjalan. Pasang agen untuk menyebar workload ke banyak mesin.",
    a_start: "Mulai server", a_restart: "Ulang server", a_stop: "Hentikan server", a_kill: "Paksa matikan server",
    a_refresh: "Segarkan", a_new_file: "File baru", a_new_folder: "Folder baru", a_pull_url: "Tarik dari URL",
    a_rename: "Ganti nama", a_more: "Aksi lain", a_run_now: "Jalankan sekarang", a_run_history: "Riwayat eksekusi",
    a_test_node: "Uji agen", a_reenroll: "Daftar ulang", a_delete_node: "Hapus agen",
    a_toggle_wh: "Alihkan webhook", a_test_ping: "Kirim ping uji", a_deliveries: "Pengiriman",
    a_open_palette: "Buka palet perintah", a_open_nav: "Buka navigasi", a_make_primary: "Jadikan utama",
    a_copy_endpoint: "Salin endpoint", a_edit_notes: "Ubah catatan", a_detach: "Lepas", a_verify: "Verifikasi checksum",
    edit_schedule: "Ubah jadwal", new_schedule: "Jadwal baru", pulling_file: "Menarik file",
    bp_new_sub: "Tentukan cara server ini dijalankan.", search: "Cari",
    protected: "Terlindungi", isolation_on: "Isolasi server aktif", filter_files: "Saring file", filter_ph: "Saring…", isolation_ns: "Isolasi namespace + cgroup",
    bp_category: "Kategori", bp_category_hint: "Untuk pengelompokan di katalog.", bp_runtime: "Runtime",
    bp_launch: "Perintah peluncuran", bp_launch_hint: "Perintah yang dijalankan di dalam direktori server ter-isolasi.",
    bp_name_hint: "Ditampilkan saat membuat server.", bp_create: "Buat blueprint", bp_copied: "VoltSpec disalin ke papan klip",
    sch_name_ph: "restart harian", sch_cron_label: "Cron", sch_tasks: "Tugas", sch_backoff: "backoff dtk", retries: "coba ulang",
    sch_cmd_ph: "perintah untuk dikirim", sch_backup_ph: "nama backup (opsional)", sch_payload_ph: "payload (opsional)", sch_payload_label: "Payload tugas",
    bk_name_opt: "Nama", bk_optional: "opsional", bk_ignore: "Pola abaikan", bk_ignore_hint: "glob gaya .gitignore, satu per baris",
    bk_ignore_note: "Dikecualikan dari arsip. Hanya server lokal — agen remote menolak pola abaikan.", bk_create_title: "Snapshot baru",
    copied: "Tersalin", moved: "Dipindahkan", extracted: "Diekstrak", downloaded: "Terunduh",
    t_perms_updated: "Izin diperbarui", t_crash_saved: "Kebijakan crash disimpan", t_crash_reset: "Burst crash disetel ulang",
    t_upload_failed: "Unggah gagal", t_install_queued: "Instalasi diantrekan", t_restored: "Dipulihkan — server disegarkan",
    t_transfer_cancelled: "Transfer dibatalkan", t_cancelling: "Membatalkan…", t_triggered: "Dipicu",
    t_primary_updated: "Endpoint utama diperbarui", t_detached: "Dilepas", t_pass_changed: "Kata sandi diubah",
    t_key_revoked: "Token dicabut", t_wh_updated: "Webhook diperbarui", t_bp_created: "Blueprint dibuat",
    e_sch_not_found: "Jadwal tidak ditemukan", e_name_cron: "Nama dan cron wajib diisi", e_one_task: "Tambah minimal satu tugas",
    e_domain_req: "Domain wajib diisi", e_valid_port: "Masukkan port yang valid (1–65535)", e_pick_cap: "Pilih minimal satu kapabilitas (atau Akses penuh)", e_required_name: "Nama wajib diisi", e_required_url: "URL wajib diisi", e_pick_user: "Pilih pengguna terlebih dahulu",
    e_node_short: "Nama agen terlalu pendek", confirm_revoke_key: "Cabut token ini? Langsung berhenti bekerja tetapi tetap terdaftar untuk audit.",
    recovery_codes: "Kode pemulihan", recovery_codes_hint: "Simpan kode sekali pakai ini di tempat aman. Setiap kode hanya berlaku satu kali — jika aplikasi autentikator hilang, kode pemulihan adalah satu-satunya jalan kembali.",
    recovery_saved: "Kode disimpan", recovery_confirm: "Saya sudah menyimpan kode",
    regenerate: "Buat ulang", disable_2fa: "Nonaktifkan 2FA", reset_2fa: "Setel ulang 2FA",
    reset_2fa_confirm: "Setel ulang 2FA untuk {name}? Kunci rahasia dan kode pemulihan akan dihapus dan pengguna harus mendaftar ulang.",
    rec_reauth: "Autentikasi ulang", rec_reauth_sub: "Masukkan kata sandi saat ini dan kode dari aplikasi autentikator untuk membuat ulang kode pemulihan.",
    recovery_use: "Gunakan kode pemulihan", recovery_ph: "XXXXX-XXXXX",
    totp_code: "Kode 6 digit", disable_2fa_body: "Masukkan kode dari aplikasi autentikator untuk mematikan autentikasi dua faktor. Kode pemulihan Anda ikut dihapus.",
    twofa_sub: "Masukkan kode dari aplikasi autentikator Anda.", use_authenticator: "Gunakan aplikasi autentikator",
    twofa_scan: "Pindai dengan aplikasi autentikator apa pun (Google Authenticator, Authy, 1Password).", twofa_secret: "Rahasia",
    token_created: "Token akses dibuat", token_once: "Ditampilkan sekali — salin sekarang, tidak bisa diambil lagi nanti.", copy_token: "Salin token", done: "Selesai",
    wh_secret_title: "Rahasia penanda tangan webhook", wh_secret_once: "Ditampilkan sekali — salin sekarang, tidak bisa diambil lagi nanti.", copy_secret: "Salin rahasia",
    qa_compose: "Buat server", qa_compose_sub: "Luncurkan dari blueprint", qa_attach: "Pasang agen", qa_attach_sub: "Perluas fabric eksekusi",
    qa_api: "Akses API", qa_api_sub: "Kelola kredensial ber-scope", qa_security: "Keamanan akun", qa_security_sub: "Kata sandi dan autentikasi dua faktor", recent_activity: "Aktivitas terbaru",
    quick_actions: "Aksi cepat", webhook_created: "Webhook dibuat",
    watchers: "Pemantau", pause: "Jeda", enable: "Aktifkan",
    watchers_badge: "reaktif", watchers_hint: "aturan pola log yang memicu aksi saat cocok",
    watchers_empty: "Belum ada pemantau — tambahkan aturan untuk merespons keluaran konsol",
    watcher_fired: "terpicu {n}× · {ago}", watcher_never: "belum pernah terpicu",
    watcher_new: "Pemantau baru", watcher_edit: "Ubah pemantau",
    watcher_name_ph: "mis. Penjaga crash", watcher_pattern: "Pola pencocokan",
    watcher_pattern_ph: "mis. Out of memory", watcher_regex: "Perlakukan pola sebagai ekspresi reguler",
    watcher_action: "Saat cocok", watcher_cooldown: "Jeda ulang", watcher_cooldown_hint: "detik tunggu sebelum terpicu lagi",
    watcher_level: "Tingkat notifikasi", watcher_command: "Perintah yang dikirim", watcher_command_ph: "mis. say Server akan dimulai ulang",
    watcher_no_payload: "Tanpa masukan tambahan — aksi ini berjalan apa adanya.",
    watcher_confirm_del: "Hapus pemantau \"{name}\"?",
    wt_notify: "Notifikasi", wt_command: "Jalankan perintah", wt_restart: "Mulai ulang", wt_stop: "Hentikan",
    lvl_info: "Info", lvl_warn: "Peringatan", lvl_error: "Kesalahan",
    e_watcher_fields: "Nama dan pola wajib diisi", e_watcher_command: "Aksi perintah butuh perintah untuk dikirim",
  },
};
const t = (k, params) => {
  let s = (I18N[state.lang] || I18N.en)[k] || I18N.en[k] || k;
  if (params) for (const p in params) s = s.replace(new RegExp("\\{" + p + "\\}", "g"), params[p]);
  return s;
};

/* ---------- helpers ---------- */
const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const fmtBytes = (b) => { b = +b || 0; if (b >= 1073741824) return (b / 1073741824).toFixed(2) + " GB"; if (b >= 1048576) return (b / 1048576).toFixed(2) + " MB"; if (b >= 1024) return (b / 1024).toFixed(2) + " KB"; return b + " B"; };
const fmtTime = (s) => { s = +s || 0; const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60); if (d) return `${d}d ${h}h`; if (h) return `${h}h ${m}m`; return `${m}m`; };
const fmtDate = (iso) => iso ? new Date(iso).toLocaleString() : "—";
/* Terminal empty-state: icon + title + hint, and an optional create action.
   Loading placeholders keep the plain `.empty` spinner-word form; this is for
   "nothing here yet — here's what it is and how to start" states. `opts.act`
   wires a data-act button; `opts.actLabel`/`opts.icon` customize it. */
const emptyState = (icon, title, hint, opts = {}) => {
  const btn = opts.act
    ? `<button class="btn primary sm" data-act="${opts.act}"${opts.data || ""}>${ic(opts.actIcon || "plus", 14)}<span>${esc(opts.actLabel || t("create"))}</span></button>`
    : "";
  return `<div class="empty">${ic(icon, 40)}<b>${esc(title)}</b>${hint ? `<p class="empty-hint">${esc(hint)}</p>` : ""}${btn}</div>`;
};
/* FileEntry.modified arrives as UNIX seconds (not ISO) — convert explicitly. */
const fmtFileDate = (secs) => { const n = +secs; return Number.isFinite(n) && n > 0 ? new Date(n * 1000).toLocaleString() : "—"; };
/* Status → pill class whitelist: a hostile agent status string must never
   become a class name (it would only style, but the map keeps the output
   deterministic and the set of visible states small). */
const STATUS_CLS = { running: "running", starting: "starting", stopping: "stopping", stopped: "stopped", offline: "offline", crashed: "crashed", suspended: "suspended", installing: "installing" };
const statusCls = (s) => STATUS_CLS[String(s ?? "").toLowerCase()] || "offline";
/* Mirror status → pill class whitelist (backend emits disabled/ok/degraded;
   anything else degrades to the muted disabled state so a hostile payload
   can never mint a class name or an i18n key). */
const MIRROR_CLS = { ok: "success", degraded: "warn", disabled: "offline" };
const mirrorState = (s) => (MIRROR_CLS[String(s ?? "")] ? String(s) : "disabled");

/* ---------- theme ---------- */
const THEME_KEY = "volt-theme";
const storedTheme = () => { try { return localStorage.getItem(THEME_KEY); } catch (e) { return null; } };
const themePref = () => storedTheme() || (window.matchMedia && matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
const applyTheme = () => { document.documentElement.dataset.theme = themePref(); };
const themeIcon = () => themePref() === "dark" ? ic("sun", 16) : ic("moon", 16);
function toggleTheme() {
  try { localStorage.setItem(THEME_KEY, themePref() === "dark" ? "light" : "dark"); } catch (e) {}
  applyTheme();
  const b = document.querySelector('[data-act="toggleTheme"]');
  if (b) b.innerHTML = themeIcon();
}

/* ---------- global activity indicator + button busy ---------- */
let apiActive = 0; // in-flight api() requests
const apiIdleCallbacks = new Set();

function activityBar() {
  let bar = document.getElementById("activity-bar");
  if (!bar) {
    bar = document.createElement("div");
    bar.id = "activity-bar";
    bar.className = "activity-bar";
    bar.setAttribute("aria-hidden", "true");
    document.body.appendChild(bar);
  }
  return bar;
}

function apiTick(delta) {
  apiActive = Math.max(0, apiActive + delta);
  activityBar().classList.toggle("on", apiActive > 0);
  if (apiActive === 0 && apiIdleCallbacks.size) {
    const cbs = [...apiIdleCallbacks]; apiIdleCallbacks.clear();
    cbs.forEach((cb) => cb());
  }
}

function apiWhenIdle(cb) { apiIdleCallbacks.add(cb); }

/* Show a spinner + disable a button while an action is in flight. The original
   innerHTML is stashed once and restored verbatim, so icon/chevron buttons come
   back exactly as they were. */
function busyButton(btn, busy) {
  if (!btn || !(btn instanceof HTMLButtonElement)) return;
  if (busy) {
    if (btn.dataset.busy === "1") return;
    btn.dataset.busy = "1";
    btn.dataset.busyHtml = btn.innerHTML;
    btn.disabled = true;
    btn.setAttribute("aria-busy", "true");
    const label = (btn.textContent || "").trim();
    btn.innerHTML = `<span class="btn-spinner" aria-hidden="true"></span>` + (label ? `<span>${esc(label)}</span>` : "");
  } else {
    if (btn.dataset.busy !== "1") return;
    delete btn.dataset.busy;
    if (btn.dataset.busyHtml !== undefined) btn.innerHTML = btn.dataset.busyHtml;
    delete btn.dataset.busyHtml;
    btn.disabled = false;
    btn.removeAttribute("aria-busy");
  }
}

/* Dispatch a delegated action with busy feedback on the triggering button.
   Actions that return a promise (or that start a request synchronously) keep
   the button busy until they settle; instant actions clear immediately. */
function runAction(el, e, fn) {
  const btn = el && el.closest ? el.closest("button") : null;
  if (!btn) { fn(el, e); return; }
  busyButton(btn, true);
  const before = apiActive;
  let settled = false;
  const finish = () => { if (!settled) { settled = true; busyButton(btn, false); } };
  try {
    const r = fn(el, e);
    if (r && typeof r.then === "function") r.then(finish, finish);
    else if (apiActive > before) apiWhenIdle(finish);
    else finish();
  } catch (err) { finish(); throw err; }
}

async function api(path, opts = {}) {
  opts.headers = Object.assign({ "Content-Type": "application/json" }, opts.headers || {});
  apiTick(1);
  // Hard timeout (default 30s, overridable per call) so a wedged request can
  // never leave a spinner or a stuck "loading" list forever.
  const ctrl = new AbortController();
  // An external signal (command-palette scoped search) cancels the request
  // early; the hard timeout still covers the no-signal path.
  const ext = opts.signal || null;
  const timer = setTimeout(() => ctrl.abort(), opts.timeout ?? 30000);
  try {
    const res = await fetch(API + path, { ...opts, signal: ext || ctrl.signal });
    const ct = res.headers.get("content-type") || "";
    if (res.status === 401 && !path.includes("login")) { renderLogin(); throw new Error("unauthorized"); }
    const body = ct.includes("application/json") ? await res.json() : await res.text();
    if (!res.ok) throw new Error(apiErrorMessage(body, res.statusText));
    return body;
  } finally { clearTimeout(timer); apiTick(-1); }
}

/* Normalize an API error body (JSON object, JSON scalar, or plain text) into a
   single human-readable message. Backend errors use `{error, status}`; other
   shapes (axum text rejections, `message`/`detail`) degrade gracefully instead
   of rendering "[object Object]" or nothing. */
function apiErrorMessage(body, statusText) {
  if (typeof body === "string" && body.trim()) return body.trim();
  if (body && typeof body === "object") {
    const msg = body.error ?? body.message ?? body.detail ?? body.msg;
    if (typeof msg === "string" && msg.trim()) return msg.trim();
  }
  return statusText || "request failed";
}

function toast(msg, kind = "info") {
  let wrap = $("#toast-wrap");
  if (!wrap) {
    wrap = document.createElement("div");
    wrap.id = "toast-wrap";
    wrap.setAttribute("role", "status");
    wrap.setAttribute("aria-live", "polite");
    document.body.appendChild(wrap);
  }
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.innerHTML = `${ic(kind === "success" ? "check" : kind === "error" ? "xcircle" : kind === "warn" ? "alert" : "info", 16)}<span>${esc(msg)}</span>`;
  wrap.appendChild(el);
  setTimeout(() => { el.style.opacity = "0"; el.style.transition = "opacity .3s"; setTimeout(() => el.remove(), 300); }, 3600);
}

function vpDialog({ title, message = "", input = null, confirmText = "Confirm", danger = false, copyValue = null }) {
  return new Promise((resolve) => {
    const modal = document.createElement("div"); modal.className = "modal dialog-layer";
    modal.innerHTML = `<div class="modal-card dialog-card"><div class="modal-head"><b>${ic(danger ? "alert" : "info",16)}${esc(title)}</b><button class="icon-btn dialog-cancel">${ic("x",16)}</button></div><div class="dialog-body">${message ? `<p>${esc(message)}</p>` : ""}${input !== null ? `<div class="field"><label>Value</label><input class="dialog-input" value="${esc(input)}"></div>` : ""}${copyValue !== null ? `<div class="code-block">${esc(copyValue)}</div>` : ""}</div><div class="modal-foot"><button class="btn ghost dialog-cancel">Cancel</button>${copyValue !== null ? `<button class="btn dialog-copy">${ic("copy",14)}<span>Copy</span></button>` : ""}<button class="btn ${danger ? "danger" : "primary"} dialog-ok">${ic(danger ? "trash" : "check",14)}<span>${esc(confirmText)}</span></button></div></div>`;
    const close = value => { modal.remove(); resolve(value); };
    modal.querySelectorAll(".dialog-cancel").forEach(el => el.addEventListener("click", () => close(false)));
    modal.querySelector(".dialog-copy")?.addEventListener("click", () => { const p = navigator.clipboard?.writeText(copyValue); if (p) p.catch(() => {}); toast(t("copied"), "success"); });
    modal.querySelector(".dialog-ok").addEventListener("click", () => close(input !== null ? modal.querySelector(".dialog-input").value : true));
    modal.addEventListener("click", e => { if (e.target === modal) close(false); }); document.body.appendChild(modal); modal.querySelector(".dialog-input")?.focus();
  });
}
const vpConfirm = (message, title = "Please confirm") => vpDialog({ title, message, danger: true });
const vpPrompt = (title, value = "") => vpDialog({ title, input: value, confirmText: "Continue" });

/* Irreversible actions get their own surface: the exact target, what is destroyed,
   and a typed confirmation. `phrase` defaults to the target name. */
function vpDestroy({ kind, target, phrase = target, consequences = [], confirmText = "Delete forever" }) {
  return new Promise((resolve) => {
    const modal = document.createElement("div"); modal.className = "modal dialog-layer";
    const list = consequences.map((c) => `<li>${ic("alert", 13)}<span>${esc(c)}</span></li>`).join("");
    modal.innerHTML = `<div class="modal-card dialog-card destroy-card">
      <div class="modal-head destroy-head"><b>${ic("trash", 16)}Delete ${esc(kind)}</b><button class="icon-btn dialog-cancel">${ic("x", 16)}</button></div>
      <div class="dialog-body">
        <div class="destroy-target">${ic(kind === "node" ? "globe" : kind === "user" ? "user" : "server", 15)}<b>${esc(target)}</b></div>
        ${list ? `<ul class="destroy-list">${list}</ul>` : ""}
        <div class="field destroy-field"><label>Type <code>${esc(phrase)}</code> to confirm</label><div class="field-input">${ic("pencil", 14)}<input class="dialog-input" autocomplete="off" spellcheck="false" placeholder="${esc(phrase)}"></div></div>
      </div>
      <div class="modal-foot"><button class="btn ghost dialog-cancel">Cancel</button><button class="btn danger dialog-ok" disabled>${ic("trash", 14)}<span>${esc(confirmText)}</span></button></div>
    </div>`;
    const close = (value) => { document.removeEventListener("keydown", onKey); modal.remove(); resolve(value); };
    const input = modal.querySelector(".dialog-input");
    const ok = modal.querySelector(".dialog-ok");
    const matches = () => input.value.trim() === phrase;
    const onKey = (e) => { if (e.key === "Escape") close(false); else if (e.key === "Enter" && matches()) close(true); };
    input.addEventListener("input", () => { ok.disabled = !matches(); });
    modal.querySelectorAll(".dialog-cancel").forEach((el) => el.addEventListener("click", () => close(false)));
    ok.addEventListener("click", () => { if (matches()) close(true); });
    modal.addEventListener("click", (e) => { if (e.target === modal) close(false); });
    document.addEventListener("keydown", onKey);
    document.body.appendChild(modal); input.focus();
  });
}

/* ---------- router + command palette ---------- */
window.addEventListener("keydown", e => { if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") { e.preventDefault(); openCommandPalette(); } });

function openCommandPalette() {
  const isRoot = !!state.user?.root_admin;
  /* Static command set: navigation, root-only create actions, account
     actions. Every row carries a `run` thunk or an `href`; keys drive the
     fuzzy match. Nothing here requires a fetch — the heavy lists below are
     loaded lazily through the scoped prefixes. */
  const cmds = [
    { sec: "nav", name: "Dashboard", icon: "activity", href: "#/", key: "pulse" },
    { sec: "nav", name: "Servers", icon: "server", href: isRoot ? "#/admin/servers" : "#/workspaces", key: "workspaces" },
    { sec: "nav", name: "Schedules", icon: "clock", href: "#/automations", key: "flows" },
    { sec: "nav", name: "Nodes", icon: "globe", href: "#/admin/nodes", key: "fabric" },
    { sec: "nav", name: "Eggs", icon: "box", href: "#/admin/blueprints", key: "blueprint studio" },
    { sec: "nav", name: "System", icon: "gauge", href: "#/admin/system", key: "observatory" },
    { sec: "nav", name: t("settings"), icon: "settings", href: "#/settings", key: "settings" },
    { sec: "nav", name: t("profile"), icon: "profile", href: "#/profile", key: "profile" },
    ...(isRoot ? [
      { sec: "nav", name: t("users"), icon: "users", href: "#/admin/users", key: "users team" },
      { sec: "nav", name: t("squads"), icon: "users", href: "#/admin/squads", key: "squads" },
      { sec: "nav", name: t("registry"), icon: "box", href: "#/admin/blueprints/registry", key: "registry packages" },
    ] : []),
    ...(isRoot ? [
      { sec: "create", name: t("palette_new_workspace"), icon: "server", run: () => adminNewServer(), key: "new workspace create server" },
      { sec: "create", name: t("palette_new_user"), icon: "user", run: () => adminNewUser(), key: "new member create user" },
      { sec: "create", name: t("palette_new_squad"), icon: "users", run: () => adminNewSquad(), key: "new squad create" },
      { sec: "create", name: t("palette_new_blueprint"), icon: "box", run: () => adminNewBlueprint(), key: "new blueprint create" },
      { sec: "create", name: t("palette_attach_node"), icon: "globe", run: () => adminNewNode(), key: "attach agent new node" },
    ] : []),
    { sec: "account", name: t("palette_theme"), icon: "sun", run: () => toggleTheme(), key: "theme dark light" },
    { sec: "account", name: t("logout"), icon: "logout", run: () => logout(), key: "logout sign out" },
  ];
  /* Scoped lazy sections: typing `s:` / `u:` / `bp:` / `ev:` fetches the
     matching existing endpoint (debounced 250ms) and renders the results
     under its own section header. Fetches are cancelable via AbortController
     and guarded by routeToken so a stale palette never writes. */
  const scopes = {
    s: { label: () => t("palette_servers"), icon: "server", href: (x) => `#/server/${x.id}`, name: (x) => x.name, sub: (x) => String(x.status || ""), fetch: () => api("/servers", { signal: activeSignal() }).then((r) => r.data || []).catch(() => []) },
    u: { label: () => t("palette_users"), icon: "user", href: (x) => `#/admin/users/${x.id}`, name: (x) => x.username, sub: () => "", fetch: () => api("/admin/users", { signal: activeSignal() }).then((r) => r.data || r || []).catch(() => []) },
    bp: { label: () => t("palette_blueprints"), icon: "blueprint", href: () => "#/admin/blueprints", name: (x) => x.name, sub: (x) => String(x.category || ""), fetch: () => api("/blueprints", { signal: activeSignal() }).then((r) => r.data || r || []).catch(() => []) },
    ev: { label: () => t("palette_events"), icon: "activity", href: null, name: (x) => x.action, sub: (x) => [x.target, x.username].filter(Boolean).join(" · "), fetch: () => api("/audit", { signal: activeSignal() }).then((r) => r.data || r || []).catch(() => []) },
  };
  const modal = document.createElement("div"); modal.className = "modal palette-layer";
  modal.innerHTML = `<div class="command-palette" role="dialog" aria-modal="true" aria-label="${esc(t("palette_search"))}">
    <div class="palette-search">${ic("search", 17)}<input placeholder="${esc(t("palette_search"))}" autocomplete="off" role="combobox" aria-expanded="true" aria-controls="palette-list" aria-autocomplete="list" aria-activedescendant=""><kbd>ESC</kbd></div>
    <div class="palette-results" id="palette-list" role="listbox" aria-label="${esc(t("palette_commands"))}"></div>
    <div class="palette-hint">${esc(t("palette_scoped_hint"))}</div>
  </div>`;
  document.body.appendChild(modal);
  const input = modal.querySelector("input");
  const list = modal.querySelector("#palette-list");
  const routeGuard = routeToken;
  let active = 0;
  let visible = [];
  let timer = null;
  let abort = null;
  let scopedRows = {}; // scope -> rows fetched during this open (cache)
  const activeSignal = () => abort?.signal;
  const close = () => { clearTimeout(timer); if (abort) abort.abort(); modal.remove(); };

  const highlight = () => {
    const opt = visible[active];
    $$(".palette-option", list).forEach((a) => a.classList.toggle("active", !!opt && a.dataset.oid === opt.oid));
    input.setAttribute("aria-activedescendant", opt ? opt.oid : "");
    if (opt) list.querySelector(`[data-oid="${opt.oid}"]`)?.scrollIntoView({ block: "nearest" });
  };

  const render = (q) => {
    if (!modal.isConnected || routeToken !== routeGuard) return;
    const toks = q.toLowerCase().split(/\s+/).filter(Boolean);
    const m = /^(s|u|bp|ev):(.*)$/i.exec(q.trim());
    const scope = m ? m[1].toLowerCase() : null;
    const subToks = m ? m[2].toLowerCase().split(/\s+/).filter(Boolean) : [];
    const hit = (text) => !toks.length || toks.every((tok) => text.includes(tok));
    const hitSub = (text) => !subToks.length || subToks.every((tok) => text.includes(tok));
    const sections = [];
    const navHit = cmds.filter((c) => hit(c.key));
    if (navHit.length) sections.push({ label: t("palette_commands"), items: navHit.map((c) => ({ icon: c.icon, name: c.name, sub: "", href: c.href, run: c.run })) });
    if (scope) {
      const sc = scopes[scope];
      const loading = scopedRows[scope] === undefined;
      const rows = loading ? [] : scopedRows[scope].filter((x) => hitSub(sc.name(x) + " " + sc.sub(x)));
      sections.push({
        label: sc.label(),
        loading,
        items: rows.map((x) => ({ icon: sc.icon, name: sc.name(x), sub: sc.sub(x), href: sc.href ? sc.href(x) : null, run: null })),
      });
    }
    const flat = [];
    let id = 0;
    list.innerHTML = sections.length
      ? sections.map((s) => `<div class="palette-section" role="presentation">${esc(s.label)}</div>` +
          (s.loading
            ? `<div class="palette-option loading" role="presentation">${ic("activity", 16)}<span>${esc(t("palette_loading"))}</span></div>`
            : s.items.map((it) => {
                const oid = `pal-opt-${id++}`;
                flat.push({ oid, ...it });
                return `<a id="${oid}" role="option" class="palette-option" data-oid="${oid}" href="${it.href ? esc(it.href) : "#"}">${ic(it.icon, 16)}<span class="palette-row-main"><b>${esc(it.name)}</b>${it.sub ? `<small>${esc(it.sub)}</small>` : ""}</span>${it.href ? "" : "<kbd>↵</kbd>"}</a>`;
              }).join(""))).join("")
      : `<div class="palette-empty" role="presentation">${ic("search", 18)}<span>${esc(t("palette_no_results"))}</span></div>`;
    visible = flat;
    active = Math.min(active, Math.max(0, visible.length - 1));
    highlight();
  };

  const loadScope = (scope) => {
    clearTimeout(timer);
    if (abort) abort.abort();
    abort = new AbortController();
    const myAbort = abort;
    timer = setTimeout(() => {
      scopes[scope].fetch()
        .then((rows) => { if (!myAbort.signal.aborted) { scopedRows[scope] = rows || []; render(input.value); } })
        .catch(() => { if (!myAbort.signal.aborted) { scopedRows[scope] = []; render(input.value); } });
    }, 250);
  };

  const runItem = (it) => {
    if (it.href) location.hash = it.href;
    else if (it.run) it.run();
  };

  const filter = () => {
    clearTimeout(timer);
    if (abort) abort.abort();
    const q = input.value;
    const m = /^(s|u|bp|ev):(.*)$/i.exec(q.trim());
    render(q);
    if (m) {
      const scope = m[1].toLowerCase();
      if (scopedRows[scope] === undefined) loadScope(scope);
    }
  };

  input.addEventListener("input", filter);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { e.stopPropagation(); close(); return; }
    if (!visible.length) return;
    if (e.key === "ArrowDown") { e.preventDefault(); active = (active + 1) % visible.length; highlight(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); active = (active - 1 + visible.length) % visible.length; highlight(); }
    else if (e.key === "Home") { e.preventDefault(); active = 0; highlight(); }
    else if (e.key === "End") { e.preventDefault(); active = visible.length - 1; highlight(); }
    else if (e.key === "Enter") { e.preventDefault(); e.stopPropagation(); const it = visible[active]; if (it) { close(); runItem(it); } }
  });
  list.addEventListener("click", (e) => {
    const a = e.target.closest("[data-oid]");
    if (!a) return;
    e.preventDefault();
    const it = visible.find((v) => v.oid === a.dataset.oid);
    if (it) { close(); runItem(it); }
  });
  modal.addEventListener("click", (e) => { if (e.target === modal) close(); });
  render("");
  input.focus();
}

let routeTransitionId = 0;
let routeToken = 0; // bumped on every route()/renderLogin(); async page renders capture it to reject stale writes
function route() {
  routeToken++;
  const app = document.getElementById("app");
  if (!state.user) { renderLogin(); return; }
  const path = location.hash.slice(1) || "/";
  const parts = path.split("/").filter(Boolean);
  const render = () => {
    killPollers();
    if (parts[0] === "server" && parts[1]) return renderServerPage(parts[1].replace(/\D/g, ""), parts[2] || "console");
    if (parts[0] === "admin") return renderAdmin(parts[1] || "servers");
    if (parts[0] === "profile") return renderProfile();
    if (parts[0] === "automations") return renderAutomations();
    if (parts[0] === "settings") return renderSettings();
    if (parts[0] === "workspaces") return renderWorkspaces();
    return renderDashboard();
  };
  if (!app.querySelector(".layout") || matchMedia("(prefers-reduced-motion: reduce)").matches) {
    render();
    return;
  }
  const id = ++routeTransitionId;
  app.classList.remove("route-enter");
  app.classList.add("route-exit");
  window.setTimeout(() => {
    if (id !== routeTransitionId) return;
    render();
    app.classList.remove("route-exit");
    app.classList.add("route-enter");
    window.setTimeout(() => app.classList.remove("route-enter"), 520);
  }, 150);
}

window.addEventListener("hashchange", () => route());
window.addEventListener("load", init);

const modalObserver = new MutationObserver((muts) => {
  /* Modals that carried a blob: URL preview hand it back when removed — every
     close path (closeModal, Escape, dirty-guard) goes through DOM removal. */
  for (const mut of muts) {
    for (const node of mut.removedNodes) {
      if (node.nodeType !== 1) continue;
      const hit = node.matches?.("[data-preview-url]") ? node : node.querySelector?.("[data-preview-url]");
      if (hit?.dataset.previewUrl) { URL.revokeObjectURL(hit.dataset.previewUrl); delete hit.dataset.previewUrl; }
    }
  }
  const modals=[...document.querySelectorAll(".modal")]; document.body.classList.toggle("modal-open",modals.length>0);
  modals.forEach(modal=>{const dialog=modal.querySelector(".modal-card,.command-palette");if(dialog&&!dialog.hasAttribute("role")){dialog.setAttribute("role","dialog");dialog.setAttribute("aria-modal","true");queueMicrotask(()=>dialog.querySelector("input,button,select,textarea,a[href]")?.focus());}});
});
modalObserver.observe(document.body,{childList:true,subtree:true});
document.addEventListener("keydown",e=>{const modal=[...document.querySelectorAll(".modal")].at(-1);if(!modal)return;if(e.key==="Escape"){const cancel=modal.querySelector(".dialog-cancel");if(cancel)cancel.click();else if(modal.querySelector('[data-act="pullCancel"]'))modal.querySelector('[data-act="pullCancel"]').click();else modal.remove();return}if(e.key==="Tab"){const focus=[...modal.querySelectorAll("button:not([disabled]),input:not([disabled]),select:not([disabled]),textarea:not([disabled]),a[href]")];if(!focus.length)return;const first=focus[0],last=focus.at(-1);if(e.shiftKey&&document.activeElement===first){e.preventDefault();last.focus()}else if(!e.shiftKey&&document.activeElement===last){e.preventDefault();first.focus()}}});
document.addEventListener("visibilitychange", () => {
  if (document.hidden) return;
  if (!state.consoleEs && document.querySelector("#console-out") && state.consoleServer) {
    renderConsole(state.consoleServer);
  }
  // Spark resume: collapse each ring to its last sample (no stale backfill),
  // then repaint the sparklines in place.
  for (const sid in state.sparks) {
    const r = state.sparks[sid];
    if (r.cpu.length > 1) { r.cpu = r.cpu.slice(-1); r.mem = r.mem.slice(-1); }
  }
  updateSparks();
});
/* Close any open file row menu when clicking elsewhere (capture: runs before
   the delegated data-act handler, so menu actions still dispatch). */
document.addEventListener("click", (e) => { if (!e.target.closest(".file-menu") || e.target.closest(".file-menu-item")) closeFileMenus(); }, true);
document.addEventListener("keydown", (e) => { if (e.key === "Enter" && e.target?.classList?.contains("f-name")) { e.preventDefault(); e.target.click(); } });

/* ---------- delegated bindings (no user data in JS strings) ---------- */
/* Every interactive control carries data-act + data-* attributes. Values are
   HTML-escaped at render time (esc()) and round-trip through the parser back
   to the original value in dataset — they NEVER become inline handler code, so
   a filename/domain/db-name cannot inject script. */
document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-act]");
  if (!el || el.hasAttribute("data-act-submit") || el.hasAttribute("data-act-change")) return;
  const fn = ACTIONS[el.dataset.act];
  if (fn) { e.preventDefault(); runAction(el, e, fn); }
});
document.addEventListener("change", (e) => {
  const el = e.target.closest("[data-act-change]");
  if (!el) return;
  const fn = ACTIONS[el.dataset.act];
  if (fn) runAction(el, e, fn);
});
document.addEventListener("submit", (e) => {
  const el = e.target.closest("[data-act-submit]");
  if (!el) return;
  const fn = ACTIONS[el.dataset.act];
  if (fn) { e.preventDefault(); fn(el, e); }
});
const ACTIONS = {
  doLogin(el) { doLogin(el); },
  do2fa(el) { do2fa(el); },
  toggleSidebar() { toggleSidebar(); },
  logout() { logout(); },
  openPalette() { openCommandPalette(); },
  refreshServers() { refreshServers(); },
  closeModal(el) { el.closest(".modal")?.remove(); },
  copyText(el) { const p = navigator.clipboard?.writeText(el.dataset.text || ""); if (p) p.catch(() => {}); toast(t("copied"), "success"); },
  power(el) { power(el.dataset.sid, el.dataset.action); },
  confirmKill(el) { confirmKill(el.dataset.sid); },
  sendCmd(el) { sendCmd(el.dataset.sid); },
  fileOpen(el) { fileOpen(el.dataset.sid, el.dataset.path); },
  fileOpenDir(el) { loadFiles(el.dataset.sid, el.dataset.path); },
  fileDl(el) { fileDl(el.dataset.sid, el.dataset.path); },
  fileRename(el) { fileRename(el.dataset.sid, el.dataset.path); },
  fileCopy(el) { fileCopy(el.dataset.sid, el.dataset.path); },
  fileDel(el) { fileDel(el.dataset.sid, el.dataset.path); },
  fileUpload(el) { fileUpload(el.dataset.sid); },
  fileNewFile(el) { fileNewFile(el.dataset.sid); },
  fileNewDir(el) { fileNewDir(el.dataset.sid); },
  fileSave(el) { fileSave(el.dataset.sid, el.dataset.path, el); },
  fileSort(el) { fileSortToggle(el.dataset.key); },
  fileToggleSel(el) { fileToggleSel(el); },
  fileMenu(el) { fileMenu(el.dataset.sid, el.dataset.path, el.dataset.dir === "1", el.dataset.ext, el); },
  fileCutSel(el) { fileCutSel(el.dataset.sid); },
  fileDelSel(el) { fileDelSel(el.dataset.sid); },
  fileZipSel(el) { fileZipSel(el.dataset.sid); },
  fileDlSel(el) { fileDlSel(el.dataset.sid); },
  fileSelNone() { fileSelNone(); },
  filePaste(el) { filePaste(el.dataset.sid); },
  fileMoveAsk(el) { fileMoveAsk(el.dataset.sid, el.dataset.path); },
  fileChmod(el) { fileChmod(el.dataset.sid, el.dataset.path); },
  fileZip(el) { fileZip(el.dataset.sid, el.dataset.path); },
  fileExtract(el) { fileExtract(el.dataset.sid, el.dataset.path); },
  dbOpen(el) { dbOpen(el.dataset.sid, el.dataset.name); },
  dbDrop(el) { dbDrop(el.dataset.sid, el.dataset.name); },
  dbExec(el) { dbExec(el.dataset.sid, el.dataset.name); },
  dbSchema(el) { dbSchema(el.dataset.sid, el.dataset.name); },
  dbSchemaCols(el) { dbSchemaCols(el.dataset.sid, el.dataset.name, el.dataset.table, el.dataset.idx); },
  dbCreate(el) { dbCreate(el.dataset.sid); },
  dbRetry() { dbLoad(state.server?.id || state.fileId); },
  bkRetry() { bkLoad(state.server?.id || state.fileId); },
  bkCreate(el) { bkCreate(el.dataset.sid); },
  bkRestore(el) { bkRestore(el.dataset.sid, +el.dataset.bid, el); },
  bkDel(el) { bkDel(+el.dataset.bid); },
  bkVerify(el) { bkVerify(+el.dataset.bid); },
  bkCleanup(el) { bkCleanup(el.dataset.sid); },
  filePull(el) { filePull(el.dataset.sid); },
  pullStart(el) { pullStart(el.dataset.sid); },
  pullCancel(el) { pullCancel(el.dataset.sid, el.dataset.tid); },
  bkDoCreate(el) { bkDoCreate(el.dataset.sid, el); },
  bkLock(el) { bkLock(+el.dataset.bid, el.dataset.on === "1"); },
  bkMirrorSync(el) { bkMirrorSync(el.dataset.sid, el); },
  schCreate(el) { schCreate(el.dataset.sid); },
  schEdit(el) { schEdit(el.dataset.sid, el.dataset.schid, el.dataset.server); },
  schSave(el) { schSave(el.dataset.sid, +el.dataset.schid, el); },
  schRuns(el) { schRuns(el.dataset.server, +el.dataset.sid); },
  schTaskAdd() { schTaskAdd(); },
  schRetry() { schLoad(state.server?.id || state.fileId); },
  schTaskDel(el) { schTaskDel(+el.dataset.idx); },
  schTaskAction(el) { schTaskAction(el); },
  schTaskGate(el) { schTaskGate(el); },
  schTaskGateField(el) { schTaskGateField(el); },
  schToggle(el) { schToggle(+el.dataset.sid, el.dataset.on === "1"); },
  schRun(el) { schRun(+el.dataset.sid); },
  schDel(el) { schDel(+el.dataset.sid); },
  watcherCreate(el) { openWatcherModal(+el.dataset.sid, null); },
  watcherEdit(el) { openWatcherModal(+el.dataset.sid, JSON.parse(decodeURIComponent(el.dataset.w))); },
  watcherSave(el) { watcherSave(+el.dataset.sid, +el.dataset.wid, el); },
  watcherToggle(el) { watcherToggle(+el.dataset.sid, JSON.parse(decodeURIComponent(el.dataset.w))); },
  watcherDel(el) { watcherDel(+el.dataset.sid, +el.dataset.wid, el.dataset.name); },
  watcherActionChange(el) { watcherActionChange(el); },
  siteOpen(el) { siteOpen(el.dataset.sid, el.dataset.hasOwnProperty('site') && el.dataset.site !== '' ? +el.dataset.site : null); },
  siteToggle(el) { siteToggle(el.dataset.sid, +el.dataset.site); },
  siteDel(el) { siteDel(el.dataset.sid, +el.dataset.site, el.dataset.domain); },
  siteSave(el) { siteSave(el.dataset.sid, el.dataset.hasOwnProperty('site') && el.dataset.site !== '' ? +el.dataset.site : null, el); },
  siteType() { siteType(); },
  consoleClear(el) { consoleClear(el.dataset.sid); },
  consoleFollow() { consoleFollow = !consoleFollow; const out = $("#console-out"); if (consoleFollow && out) out.scrollTop = out.scrollHeight; updateFollowBtn(); },
  siteRetry() { siteLoad(state.server?.id || state.fileId); },
  allocRetry() { allocLoad(state.server?.id || state.fileId); },
  keyRetry() { keyLoad(); },
  whRetry() { whLoad(); },
  automationRetry() { renderAutomations(); },
  toggleTheme() { toggleTheme(); },
  keyNew() { keyNew(); },
  keyCreate(el) { keyCreate(el); },
  keyRevoke(el) { keyRevoke(+el.dataset.id); },
  keyDel(el) { keyDel(+el.dataset.id); },
  whNew() { whNew(); },
  whEdit(el) { whEdit(+el.dataset.id); },
  whSave(el) { whSave(+el.dataset.id, el); },
  whToggle(el) { whToggle(+el.dataset.id); },
  whTest(el) { whTest(+el.dataset.id); },
  whDel(el) { whDel(+el.dataset.id); },
  whDeliveries(el) { whDeliveries(+el.dataset.id); },
  saveProfile() { saveProfile(); },
  savePass() { savePass(); },
  setup2fa() { setup2fa(); },
  confirm2fa(el) { confirm2fa(el.dataset.secret, el); },
  saveRuntime(el) { saveRuntime(el.dataset.sid); },
  saveVars(el) { saveVars(el.dataset.sid); },
  subAdd(el) { subAdd(el.dataset.sid); },
  subDel(el) { subDel(el.dataset.sid, +el.dataset.sub); },
  install(el) { install(el.dataset.sid); },
  suspend(el) { suspend(el.dataset.sid); },
  delServer(el) { delServer(el.dataset.sid); },
  loadMetrics(el) { loadMetrics(el.dataset.sid, el.dataset.win); },
  bpInspect(el) { bpInspect(+el.dataset.id); },
  bpLoadRevisions(el) { bpLoadRevisions(+el.dataset.id); },
  bpLoadDrift(el) { bpLoadDrift(+el.dataset.id); },
  bpRevDetail(el) { bpRevDetail(+el.dataset.id, +el.dataset.version, el); },
  bpRollback(el) { bpRollback(+el.dataset.id, +el.dataset.version); },
  bpPin(el) { bpPin(el, +el.dataset.bpid); },
  bpUnpin(el) { bpUnpin(+el.dataset.server, +el.dataset.bpid); },
  regInstall(el) { regInstall(el); },
  regPublish(el) { regPublish(el); },
  regGenKey() { regGenKey(); },
  regClearKey() { regClearKey(); },
  adminBlueprintExport(el) { adminBlueprintExport(+el.dataset.id); },
  adminDeleteBlueprint(el) { adminDeleteBlueprint(+el.dataset.id, el.dataset.name); },
  adminNewBlueprint() { adminNewBlueprint(); },
  adminCreateBlueprint(el) { adminCreateBlueprint(el); },
  nodeTest(el) { nodeTest(+el.dataset.id); },
  nodeReenroll(el) { nodeReenroll(+el.dataset.id); },
  nodeDelete(el) { nodeDelete(+el.dataset.id, el.dataset.name); },
  saveToggle(el) { saveToggle(el.dataset.sid, el.dataset.field, el.checked); },
  adminNewNode() { adminNewNode(); },
  nodeCreate(el) { nodeCreate(el); },
  notifRetry() { notifLoad(); },
  notifToggle() { notifToggle(); },
  notifItem(el) { notifItem(el); },
  notifClear() { notifClear(); },
  adminServersRetry() { adminServers(); },
  adminUsersRetry() { adminUsers(); },
  adminBlueprintsRetry() { adminBlueprints(); },
  adminNodesRetry() { adminNodes(); },
  adminSystemRetry() { adminSystem(); },
  adminNewServer() { adminNewServer(); },
  adminCreateServer(el) { adminCreateServer(el); },
  runwayDismiss() { runwayDismiss(); },
  adminToggleSuspend(el) { adminToggleSuspend(+el.dataset.id, el.dataset.on === "1"); },
  adminDelServer(el) { adminDelServer(+el.dataset.id, el.dataset.name); },
  adminNewUser() { adminNewUser(); },
  adminCreateUser() { adminCreateUser(); },
  adminToggleUser(el) { adminToggleUser(+el.dataset.id, el.dataset.field, el.dataset.val === "1"); },
  adminDeleteUser(el) { adminDeleteUser(+el.dataset.id, el.dataset.name); },
  allocAdd(el) { allocAdd(el.dataset.sid); },
  allocPromote(el) { allocPromote(el.dataset.sid, +el.dataset.aid); },
  allocNotes(el) { allocNotes(el.dataset.sid, +el.dataset.aid, el.dataset.port); },
  allocDel(el) { allocDel(el.dataset.sid, +el.dataset.aid, el.dataset.port); },
  adminSquadsRetry() { adminSquads(); },
  adminNewSquad() { adminNewSquad(); },
  adminCreateSquad() { adminCreateSquad(); },
  adminEditSquad(el) { adminEditSquad(el); },
  adminSaveSquad(el) { adminSaveSquad(el); },
  adminDeleteSquad(el) { adminDeleteSquad(el); },
  adminSquadRetry() { adminSquadDetail(+((location.hash.slice(1).split("/").filter(Boolean))[2] || 0)); },
  adminUserDetailRetry() { adminUserDetail(+((location.hash.slice(1).split("/").filter(Boolean))[2] || 0)); },
  sqMemberAdd(el) { sqMemberAdd(el); },
  sqMemberRole(el) { sqMemberRole(el); },
  sqMemberDel(el) { sqMemberDel(el); },
  sqServersSave(el) { sqServersSave(el); },
  fleetToggleSel(el) { fleetToggleSel(el); },
  fleetSelAll(el) { fleetSelAll(el); },
  fleetSelClear() { fleetSelClear(); },
  fleetSort(el) { fleetSortToggle(el.dataset.key); },
  fleetBulk(el) { fleetBulk(el.dataset.key); },
  adminNodeRetry() { adminNodeDetail(+((location.hash.slice(1).split("/").filter(Boolean))[2] || 0)); },
  adminNodeSave(el) { adminNodeSave(+el.dataset.id, el); },
  nodeDrain(el) { nodeDrain(+el.dataset.id, el.dataset.mode, el); },
  nodeDrainClear(el) { nodeDrainClear(+el.dataset.id); },
  regenerateRecovery() { regenerateRecovery(); },
  regenerateRecoveryDo(el) { regenerateRecoveryDo(el); },
  disable2faPrompt() { disable2faPrompt(); },
  disable2faDo(el) { disable2faDo(el); },
  recoveryCopy(el) { recoveryCopy(el); },
  recoveryDownload(el) { recoveryDownload(el); },
  adminReset2fa(el) { adminReset2fa(+el.dataset.id, el.dataset.name); },
};
async function init() {
  applyTheme();
  if (window.matchMedia) matchMedia("(prefers-color-scheme: dark)").addEventListener?.("change", () => { if (!storedTheme()) applyTheme(); });
  try {
    const me = await api("/me");
    state.user = me;
    state.lang = me.language || "en";
    document.documentElement.lang = state.lang;
    route();
    notifInit();
  } catch (e) {
    renderLogin();
  }
}
function killPollers() { state.pollers.forEach((p) => clearInterval(p)); state.pollers = []; if (state.consoleEs) { state.consoleEs.close(); state.consoleEs = null; } state.consoleServer = null; }
function poll(fn, ms) { fn(); state.pollers.push(setInterval(fn, ms)); }

/* ---------- layout ---------- */
function shell(active, title, inner) {
  const u = state.user;
  const initial = (u.username || "?").slice(0, 1).toUpperCase();
  return `<div class="layout">
    <div class="sidebar-backdrop" data-act="toggleSidebar"></div>
    <aside class="sidebar" id="sidebar">
      <div class="brand">
        <span class="brand-dot" aria-hidden="true"></span>
        <span class="brand-name">VoltPanel</span>
      </div>
      <div class="nav-group-label">CONTROL</div>
      <nav>
        <a href="#/" class="nav-item ${active === "pulse" ? "active" : ""}"><span class="nav-ico">${ic("activity")}</span><span>Dashboard</span></a>
        <a href="${u.root_admin?'#/admin/servers':'#/workspaces'}" class="nav-item ${active === "workspaces" ? "active" : ""}"><span class="nav-ico">${ic("server")}</span><span>Servers</span></a>
        <a href="#/automations" class="nav-item ${active === "flows" ? "active" : ""}"><span class="nav-ico">${ic("clock")}</span><span>Schedules</span></a>
      </nav>
      ${u.root_admin ? `<div class="nav-group-label">PLATFORM</div><nav><a href="#/admin/nodes" class="nav-item ${active === "fabric" ? "active" : ""}"><span class="nav-ico">${ic("globe")}</span><span>Nodes</span></a><a href="#/admin/blueprints" class="nav-item ${active === "blueprints" ? "active" : ""}"><span class="nav-ico">${ic("box")}</span><span>Eggs</span></a><a href="#/admin/system" class="nav-item ${active === "observatory" ? "active" : ""}"><span class="nav-ico">${ic("gauge")}</span><span>System</span></a></nav>` : ""}
      <div class="nav-group-label">ACCOUNT</div><nav><a href="#/settings" class="nav-item ${active === "settings" ? "active" : ""}"><span class="nav-ico">${ic("settings")}</span><span>${t("settings")}</span></a><a href="#/profile" class="nav-item ${active === "profile" ? "active" : ""}"><span class="nav-ico">${ic("profile")}</span><span>${t("profile")}</span></a></nav>
      <div class="side-foot">
        <div class="avatar">${esc(initial)}</div>
        <div class="side-user"><div class="name">${esc(u.username)}</div><div class="role">${u.root_admin ? "Administrator" : "Member"}</div></div>
        <button class="icon-btn" title="${t("logout")}" data-act="logout">${ic("logout", 17)}</button>
      </div>
    </aside>
    <main class="main">
      <header class="topbar">
        <div class="row"><button class="burger icon-btn" aria-label="${t("a_open_nav")}" data-act="toggleSidebar">${ic("menu", 20)}</button><h1>${esc(title)}</h1></div>
        <div class="topbar-right">
          <button class="palette-trigger" aria-label="${t("a_open_palette")}" data-act="openPalette">${ic("search",15)}<span>${t("search")}</span><kbd>Ctrl K</kbd></button>
          ${u.root_admin ? `<div class="notif-wrap">
            <button class="icon-btn notif-bell" aria-label="${t("notif_open")}" aria-haspopup="true" aria-expanded="false" data-act="notifToggle">${ic("bell", 17)}<span class="notif-badge" ${state.unread ? "" : "hidden"}>${state.unread > 99 ? "99+" : state.unread}</span></button>
            <div class="notif-panel" id="notif-panel" role="region" aria-label="${t("notif_open")}" hidden>
              <div class="notif-head"><b>${t("notifications")}</b><span class="notif-unread"></span></div>
              <div class="notif-list" id="notif-panel-list"><div class="empty notif-empty">${ic("bell", 32)}<p>${t("notif_empty")}</p></div></div>
            </div>
          </div>` : ""}
          <button class="icon-btn theme-toggle" aria-label="${t("palette_theme")}" title="${t("palette_theme")}" data-act="toggleTheme">${themeIcon()}</button>
        </div>
      </header>
      <div class="content" id="main-content" tabindex="-1">${inner}</div>
    </main>
  </div>`;
}

function toggleSidebar() { $("#sidebar")?.classList.toggle("open"); $(".sidebar-backdrop")?.classList.toggle("show"); }

async function logout() { notifTeardown(); try { await api("/logout", { method: "POST" }); } catch (e) {} state.user = null; state.server = null; state.servers = []; location.hash = ""; renderLogin(); }

/* ============================================================
   AUTH
   ============================================================ */
function renderLogin(err) {
  routeToken++;
  notifTeardown();
  killPollers();
  state.pendingLogin = null;
  state.page = "login";
  applyTheme();
  document.getElementById("app").innerHTML = `<div class="auth-wrap">
    <aside class="auth-side">
      <div class="auth-side-inner">
        <div class="auth-wordmark"><span aria-hidden="true"></span><strong>VoltPanel</strong></div>
        <h1 class="auth-title">${t("welcome")}</h1>
        <p class="auth-sub">${t("auth_tagline")}</p>
        <div class="auth-feats" aria-label="Features">
          <div class="auth-feat">${ic("zap", 16)}<span>${t("auth_feat_1")}</span></div>
          <div class="auth-feat">${ic("gauge", 16)}<span>${t("auth_feat_2")}</span></div>
          <div class="auth-feat">${ic("shield", 16)}<span>${t("auth_feat_3")}</span></div>
        </div>
      </div>
    </aside>
    <main class="auth-main">
      <div class="auth-mobile-brand"><span aria-hidden="true"></span><strong>VoltPanel</strong></div>
      <form class="auth-card" data-act="doLogin" data-act-submit>
        <header class="auth-card-copy">
          <h1>${t("auth_signin")}</h1>
          <p>${t("auth_signin_sub")}</p>
        </header>
        ${err ? `<div class="toast error auth-error">${ic("xcircle", 16)}<span>${esc(err)}</span></div>` : ""}
        <div class="auth-field">
          <label for="l-user">${t("username")}</label>
          <input class="auth-input" id="l-user" type="text" autocomplete="username" placeholder="Enter your username" required autofocus>
        </div>
        <div class="auth-field">
          <label for="l-pass">${t("password")}</label>
          <input class="auth-input" id="l-pass" type="password" autocomplete="current-password" placeholder="Enter your password" required>
        </div>
        <div class="auth-options"><label class="check-row"><input type="checkbox" id="l-rem" checked><span class="check-box">${ic("check", 13, 2.4)}</span><span>${t("remember")}</span></label></div>
        <button class="btn primary block auth-submit" type="submit"><span>${t("login")}</span>${ic("chevron_right", 16)}</button>
      </form>
      <div class="auth-foot">${t("auth_foot")}</div>
    </main>
  </div>`;
  $("#l-user").focus();
}

function setAuthBusy(form, busy, label) {
  if (!form) return false;
  if (busy && form.dataset.busy === "1") return false;
  form.dataset.busy = busy ? "1" : "0";
  form.setAttribute("aria-busy", String(busy));
  form.querySelectorAll("input, button").forEach((control) => { control.disabled = busy; });
  const submit = form.querySelector(".auth-submit");
  if (submit) {
    if (!submit.dataset.idleHtml) submit.dataset.idleHtml = submit.innerHTML;
    submit.innerHTML = busy
      ? `<span class="auth-spinner" aria-hidden="true"></span><span>${esc(label)}</span>`
      : submit.dataset.idleHtml;
  }
  return true;
}

function completeLogin(user) {
  state.pendingLogin = null;
  state.user = user;
  state.lang = user.language || "en";
  document.documentElement.lang = state.lang;
  notifInit();
  if (location.hash === "#/") route();
  else location.hash = "#/";
}

async function doLogin(form) {
  if (!setAuthBusy(form, true, t("signing_in"))) return;
  try {
    const username = $("#l-user").value;
    const password = $("#l-pass").value;
    const remember = $("#l-rem").checked;
    const res = await api("/login", {
      method: "POST",
      body: JSON.stringify({ username, password, remember }),
    });
    if (res.needs_2fa) {
      state.pendingLogin = { username, password, remember };
      render2fa();
      return;
    }
    completeLogin(res.user);
  } catch (err) {
    toast(err.message, "error");
    if (form.isConnected) setAuthBusy(form, false, "");
  }
}

function render2fa() {
  document.getElementById("app").innerHTML = `<div class="auth-wrap auth-single"><main class="auth-main">
    <div class="auth-mobile-brand"><span aria-hidden="true"></span><strong>VoltPanel</strong></div>
    <form class="auth-card" data-act="do2fa" data-act-submit>
      <header class="auth-card-copy"><h1>${t("twofa")}</h1><p>${t("twofa_sub")}</p></header>
      <div class="auth-field" id="2fa-totp-field">
        <label for="l-totp">${t("totp_code")}</label>
        <input class="auth-input auth-code" id="l-totp" inputmode="numeric" autocomplete="one-time-code" maxlength="6" placeholder="000000" required autofocus>
      </div>
      <div class="auth-field" id="2fa-recovery-field" hidden>
        <label for="l-recovery">${t("recovery_codes")}</label>
        <input class="auth-input" id="l-recovery" autocomplete="one-time-code" placeholder="${t("recovery_ph")}" spellcheck="false">
      </div>
      <p class="muted m-10-0"><button type="button" class="link-btn" id="2fa-toggle" aria-expanded="false">${t("recovery_use")}</button></p>
      <button class="btn primary block auth-submit" type="submit"><span>${t("verify")}</span>${ic("chevron_right", 16)}</button>
    </form>
    <div class="auth-foot">${t("auth_foot")}</div>
  </main></div>`;
  const toggle = $("#2fa-toggle");
  if (toggle) toggle.addEventListener("click", () => {
    const recoveryOn = !$("#2fa-recovery-field").hidden;
    $("#2fa-recovery-field").hidden = recoveryOn;
    $("#2fa-totp-field").hidden = !recoveryOn;
    toggle.setAttribute("aria-expanded", String(!recoveryOn));
    toggle.textContent = recoveryOn ? t("recovery_use") : t("use_authenticator");
    (recoveryOn ? $("#l-totp") : $("#l-recovery"))?.focus();
  });
}

async function do2fa(form) {
  const pending = state.pendingLogin;
  if (!pending) { renderLogin("Login challenge expired"); return; }
  if (!setAuthBusy(form, true, t("verifying"))) return;
  try {
    const body = { ...pending, totp_code: $("#l-totp").value };
    const recovery = $("#l-recovery")?.value?.trim();
    if (recovery) body.recovery_code = recovery;
    const res = await api("/login", { method: "POST", body: JSON.stringify(body) });
    completeLogin(res.user);
  } catch (err) {
    toast(err.message, "error");
    if (form.isConnected) setAuthBusy(form, false, "");
  }
}
async function renderDashboard() {
  state.sparkKey = null; // sentinel: even an empty fleet must render on the first poll tick
  document.getElementById("app").innerHTML = shell("pulse", "Dashboard", `
    <div class="grid ${state.user.root_admin ? "cols-4" : "cols-1"}" id="d-stats">
      <div class="card stat-card"><span class="stat-ico accent">${ic("server", 20)}</span><div class="stat-label">${t("servers")}</div><div class="stat-value" id="d-count">…</div><div class="stat-sub">active instances</div></div>
      ${state.user.root_admin ? `
      <div class="card stat-card"><span class="stat-ico green">${ic("zap", 20)}</span><div class="stat-label">${t("cpu")}</div><div class="stat-value" id="d-cpu">…</div><div class="stat-sub" id="d-load"></div></div>
      <div class="card stat-card"><span class="stat-ico purple">${ic("memory", 20)}</span><div class="stat-label">${t("ram")}</div><div class="stat-value" id="d-mem">…</div><div class="stat-bar"><div id="d-mem-bar"></div></div></div>
      <div class="card stat-card"><span class="stat-ico yellow">${ic("harddisk", 20)}</span><div class="stat-label">${t("disk")}</div><div class="stat-value" id="d-disk">…</div><div class="stat-bar"><div id="d-disk-bar"></div></div></div>` : ""}
    </div>
    <div class="card">
      <div class="card-head"><h3>${t("all_servers")}</h3><button class="icon-btn" title="refresh" data-act="refreshServers">${ic("refresh_ccw", 16)}</button></div>
      <div id="d-servers"><div class="empty">${ic("box", 40)}<p>${t("loading")}</p></div></div>
    </div>
    <div class="dashboard-lower">
      <section class="card quick-panel"><div class="card-head"><h3>${t("quick_actions")}</h3></div><div class="quick-grid">
        ${state.user.root_admin ? `<a href="#/admin/servers" class="quick-action">${ic("plus",18)}<span><b>${t("qa_compose")}</b><small>${t("qa_compose_sub")}</small></span></a><a href="#/admin/nodes" class="quick-action">${ic("globe",18)}<span><b>${t("qa_attach")}</b><small>${t("qa_attach_sub")}</small></span></a>` : ""}
        <a href="#/settings" class="quick-action">${ic("key",18)}<span><b>${t("qa_api")}</b><small>${t("qa_api_sub")}</small></span></a><a href="#/profile" class="quick-action">${ic("shield",18)}<span><b>${t("qa_security")}</b><small>${t("qa_security_sub")}</small></span></a>
      </div></section>
      <section class="card activity-panel"><div class="card-head"><h3>${ic("activity",15)} ${t("recent_activity")}</h3></div><div id="d-activity" class="activity-list"><div class="skeleton sk-act"></div><div class="skeleton sk-act"></div></div></section>
    </div>`);
  let busy = false;
  const load = async () => {
    if (busy) return; // a slow tick is still in flight — skip this one
    const token = routeToken;
    busy = true;
    try {
      const list = await api("/servers");
      if (token !== routeToken) return;
      const servers = list.data || [];
      state.servers = servers; // command palette reads the live fleet
      $("#d-count").textContent = servers.length;

      // Fleet data is the dashboard's primary content. Render it immediately;
      // host metrics are slower and must never leave this section on Loading.
      const key = servers.map((s) => `${s.id}:${s.status}`).join("|");
      if (key !== state.sparkKey) {
        state.sparkKey = key;
        await renderServerTable(servers);
      } else {
        pushServerSamples(servers);
        updateSparks();
        updateServerCells(servers);
      }

      if (document.hidden || token !== routeToken) return;
      if (state.user.root_admin) {
        api("/system/stats").then((stats) => {
          if (token !== routeToken || document.hidden) return;
          $("#d-cpu").textContent = Math.round(stats.cpu.usage_percent) + "%";
          $("#d-load").textContent = `load ${stats.load["1"].toFixed(2)}`;
          $("#d-mem").textContent = Math.round(stats.memory.percent) + "%";
          $("#d-mem-bar").style.width = Math.min(100, stats.memory.percent) + "%";
          $("#d-disk").textContent = Math.round(stats.disk.percent) + "%";
          $("#d-disk-bar").style.width = Math.min(100, stats.disk.percent) + "%";
        }).catch(() => {});
        api("/audit").then((r) => {
          if (token === routeToken) renderActivity((r.data || []).slice(0, 6));
        }).catch(() => { if (token === routeToken) renderActivity([]); });
      } else renderActivity([]);
    } catch (e) {
      if (token !== routeToken) return;
      const box = $("#d-servers");
      if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_servers")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="refreshServers">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
    }
    finally { busy = false; }
  };
  poll(load, 5000);
}
/* ---------- launch runway (empty-fleet onboarding) ---------- */
const RUNWAY_KEY = "volt-runway-dismissed";
const runwayDismissed = () => { try { return localStorage.getItem(RUNWAY_KEY) === "1"; } catch (e) { return false; } };
function runwayDismiss() {
  try { localStorage.setItem(RUNWAY_KEY, "1"); } catch (e) {}
  const box = $("#d-servers");
  if (box) box.innerHTML = `<div class="empty">${ic("box", 40)}<p>${t("no_servers")}</p></div>`;
}
let runwayNodes = -1, runwayNodesAt = 0;
async function runwayNodeCount() {
  if (!state.user?.root_admin) return 0;
  if (runwayNodes < 0 || Date.now() - runwayNodesAt > 60000) {
    const v = await api("/nodes").then((r) => (r.data || []).length).catch(() => 0);
    runwayNodes = v; runwayNodesAt = Date.now();
  }
  return runwayNodes;
}
async function runwayHTML() {
  const nodes = await runwayNodeCount();
  const steps = [
    { done: state.servers.length > 0, icon: "box", title: t("runway_workspace"), desc: t("runway_workspace_desc"),
      cta: state.user?.root_admin ? `<button class="btn primary sm" data-act="adminNewServer">${ic("plus", 13)}<span>${t("runway_compose")}</span></button>` : `<span class="badge">${t("runway_ask_admin")}</span>` },
    { done: nodes > 0, icon: "globe", title: t("runway_node"), desc: t("runway_node_desc"),
      cta: state.user?.root_admin ? `<button class="btn sm" data-act="adminNewNode">${ic("plus", 13)}<span>${t("runway_attach")}</span></button>` : "" },
    { done: false, icon: "archive", title: t("runway_backup"), desc: t("runway_backup_desc"),
      cta: `<span class="badge warn">${t("runway_waiting")}</span>` },
  ];
  const done = steps.filter((s) => s.done).length;
  return `<div class="runway">
    <div class="runway-head">
      <div class="runway-title">${ic("activity", 16)}<div><b>${t("runway_title")}</b><span>${t("runway_sub")}</span></div></div>
      <div class="runway-progress"><div class="runway-progress-bar"><i id="runway-progress-fill" data-pct="${Math.round((done / steps.length) * 100)}"></i></div><span>${done} / ${steps.length} · ${t("runway_progress")}</span></div>
      <button class="icon-btn sm" title="${t("runway_dismiss")}" aria-label="${t("runway_dismiss")}" data-act="runwayDismiss">${ic("x", 14)}</button>
    </div>
    <div class="runway-steps">${steps.map((s) => `
      <div class="runway-step${s.done ? " done" : ""}">
        <span class="runway-step-ico">${s.done ? ic("check", 15) : ic(s.icon, 15)}</span>
        <div class="runway-step-main"><b>${esc(s.title)}</b><span>${esc(s.desc)}</span></div>
        <span class="runway-step-cta">${s.cta}</span>
      </div>`).join("")}
    </div>
  </div>`;
}

async function renderServerTable(servers) {
  const box = $("#d-servers");
  if (!box) return;
  if (!servers.length) {
    box.innerHTML = runwayDismissed() ? `<div class="empty">${ic("box", 40)}<p>${t("no_servers")}</p></div>` : await runwayHTML();
    const rwFill = document.getElementById("runway-progress-fill");
    if (rwFill) rwFill.style.width = rwFill.dataset.pct + "%";
    return;
  }
  pushServerSamples(servers);
  const spark = (sid, metric, color) => {
    const r = state.sparks[sid];
    const data = r ? (metric === "mem" ? r.mem : r.cpu) : [];
    return sparkSvg(data, color, ` class="spark" data-sid="${sid}" data-m="${metric}"`);
  };
  box.innerHTML = `<div class="tbl-wrap"><table class="tbl">
    <thead><tr><th>${t("name")}</th><th>${t("status")}</th><th>${t("cpu")}</th><th>${t("ram")}</th><th>Disk</th><th>${t("uptime")}</th><th></th></tr></thead>
    <tbody>${servers.map((s) => `<tr>
      <td><a href="#/server/${s.id}" class="link-strong">${esc(s.name)}</a><div class="tbl-sub">${esc(s.blueprint)}</div></td>
      <td><span class="pill ${statusCls(s.status)}"><i></i>${esc(s.status)}</span></td>

      <td data-cpu="${s.id}">${s.info ? `<span class="cell-val">${Math.round(s.info.cpu_percent)}%</span><span class="spark-wrap">${spark(s.id, "cpu", "accent")}</span>` : "—"}</td>
      <td data-ram="${s.id}">${s.info ? `<span class="cell-val">${fmtBytes(s.info.memory_bytes)} / ${fmtBytes(s.memory_mb * 1048576)}</span><span class="spark-wrap">${spark(s.id, "mem", "purple")}</span>` : "—"}</td>
      <td data-disk="${s.id}">${s.info ? fmtBytes(s.info.disk_usage_bytes) + " / " + fmtBytes(s.disk_mb * 1048576) : "—"}</td>
      <td data-up="${s.id}">${s.info ? fmtTime(s.info.uptime_secs) : "—"}</td>
      <td><div class="actions"><a class="btn sm ghost" href="#/server/${s.id}/console">${ic("play", 14)}<span>${t("console")}</span></a></div></td>
    </tr>`).join("")}</tbody></table></div>`;
}

/* ---------- live server-list sparklines ---------- */
/* 30-sample ring per server (≈2.5 min at the 5 s dashboard poll). The table is
   rebuilt only when the fleet (ids/status) changes; every other tick updates
   the spark polyline/polygon points in place (updateSparks) — never innerHTML. */
const pushServerSamples = (servers) => {
  if (document.hidden) return; // paused off-screen — no samples, so no stale backfill
  for (const s of servers) {
    if (!s.info || !s.id) continue;
    const r = state.sparks[s.id] || (state.sparks[s.id] = { cpu: [], mem: [] });
    const cpu = typeof s.info.cpu_percent === "number" && Number.isFinite(s.info.cpu_percent) ? s.info.cpu_percent : 0;
    const mem = s.memory_mb > 0 ? Math.min(100, (s.info.memory_bytes / (s.memory_mb * 1048576)) * 100) : 0;
    r.cpu.push(cpu); r.mem.push(mem);
    if (r.cpu.length > 30) { r.cpu.shift(); r.mem.shift(); }
  }
};
function updateSparks() {
  if (document.hidden) return;
  $$(".spark[data-sid]").forEach((svg) => {
    const r = state.sparks[svg.dataset.sid];
    if (!r) return;
    const data = svg.dataset.m === "mem" ? r.mem : r.cpu;
    if (!data.length) return;
    const { pts, w, h } = sparkPoints(data);
    const poly = svg.querySelector("polyline"), fill = svg.querySelector("polygon");
    if (poly) poly.setAttribute("points", pts);
    if (fill) fill.setAttribute("points", `0,${h} ${pts} ${w},${h}`);
  });
}
/* In-place refresh of the numeric cells when the fleet is unchanged. */
const updateServerCells = (servers) => {
  if (document.hidden) return;
  for (const s of servers) {
    const set = (attr, text) => {
      const td = $(`[data-${attr}="${s.id}"]`);
      if (!td) return;
      const val = td.querySelector(".cell-val");
      if (val) val.textContent = text; else td.textContent = text;
    };
    set("cpu", s.info ? Math.round(s.info.cpu_percent) + "%" : "—");
    set("ram", s.info ? fmtBytes(s.info.memory_bytes) + " / " + fmtBytes(s.memory_mb * 1048576) : "—");
    set("disk", s.info ? fmtBytes(s.info.disk_usage_bytes) + " / " + fmtBytes(s.disk_mb * 1048576) : "—");
    set("up", s.info ? fmtTime(s.info.uptime_secs) : "—");
  }
};

function renderActivity(items) {
  const box = $("#d-activity"); if (!box) return;
  if (!items.length) { box.innerHTML = `<div class="context-empty">${ic("activity",24)}<div><b>No recent control-plane events</b><span>Power, security and provisioning actions will appear here.</span></div></div>`; return; }
  box.innerHTML = items.map(item => `<div class="activity-item"><span class="activity-icon">${ic(item.action?.includes("delete")?"trash":item.action?.includes("login")?"user":item.action?.includes("server")?"server":"activity",14)}</span><div><b>${esc(item.action||"event")}</b><span>${esc(item.target||"system")} · ${fmtDate(item.created_at)}</span></div></div>`).join("");
}

async function refreshServers() {
  try { const l = await api("/servers"); renderServerTable(l.data || []); }
  catch (e) {
    const box = $("#d-servers");
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_refresh_servers")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="refreshServers">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}

/* Dedicated workspace list for non-admin members: the dashboard's Pulse view is
   the fleet overview, while this page is the focused server list (admin users
   instead land on the Control Center fleet table). */
async function renderWorkspaces() {
  state.sparkKey = null;
  document.getElementById("app").innerHTML = shell("workspaces", t("servers"), `
    <div class="card">
      <div class="card-head"><h3>${t("all_servers")}</h3><button class="icon-btn" title="refresh" data-act="refreshServers">${ic("refresh_ccw", 16)}</button></div>
      <div id="d-servers"><div class="empty">${ic("box", 40)}<p>${t("loading")}</p></div></div>
    </div>`);
  await refreshServers();
}

/* ---------- automations ---------- */
async function renderAutomations(){
  document.getElementById("app").innerHTML=shell("flows","Schedules",`<section class="nodes-header"><div><span class="eyebrow">${t("flows_eyebrow")}</span><h2>${t("flows_title")}</h2><p>${t("flows_sub")}</p></div></section><div id="automation-grid" class="node-grid"><div class="skeleton sk-flow"></div><div class="skeleton sk-flow"></div></div>`);
  try{const servers=(await api("/servers")).data||[];const groups=await Promise.all(servers.map(async server=>({server,schedules:(await api(`/servers/${server.id}/schedules`)).data||[]})));const all=groups.flatMap(g=>g.schedules.map(schedule=>({schedule,server:g.server})));const box=$("#automation-grid");box.innerHTML=all.length?all.map(({schedule,server})=>`<article class="node-card"><div class="node-card-head"><div class="node-mark">${ic("clock",18)}</div><div><h3>${esc(schedule.name)}</h3><span>${esc(server.name)}</span></div><span class="pill ${schedule.enabled?'running':'offline'}"><i></i>${schedule.enabled?'active':'paused'}</span></div><div class="node-endpoint">${ic("terminal",13)}<code>${esc(schedule.cron_expr)}</code></div><div class="metric-line"><span>Next run</span><b>${fmtDate(schedule.next_run_at)}</b></div><div class="node-card-foot"><span>${schedule.tasks?.length||0} tasks</span><a class="btn sm ghost" href="#/server/${server.id}/schedules">Open</a></div></article>`).join(""):`<div class="context-empty">${ic("clock",28)}<div><b>No schedules</b><span>Create a schedule from a server.</span></div></div>`;}catch(e){const g=$("#automation-grid");if(g)g.innerHTML=`<div class="node-grid"><div class="card"><div class="file-error">${ic("alert",26)}<div><b>${t("err_load_flows")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="automationRetry">${ic("refresh_ccw",13)}<span>${t("retry")}</span></button></div></div></div>`;}
}

/* ============================================================
   SERVER WORKSPACE
   ============================================================ */
async function renderServerPage(id, tab) {
  const token = routeToken; // stale if a newer route()/renderLogin() superseded us while awaiting
  document.getElementById("app").innerHTML = shell("workspaces", "Server", `<div class="empty">${ic("server", 40)}<p>${t("loading")}</p></div>`);
  let data;
  try { data = await api(`/servers/${id}`); } catch (e) { if (token !== routeToken) return; document.querySelector(".content").innerHTML = `<div class="toast error">${ic("xcircle", 16)}<span>${esc(e.message)}</span></div>`; return; }
  if (token !== routeToken) return;
  const s = data.server;
  const tabs = [
    ["console", ic("terminal", 15) + t("console")], ["files", ic("folder", 15) + t("files")],
    ["databases", ic("database", 15) + t("databases")], ["backups", ic("archive", 15) + t("backups")],
    ["schedules", ic("clock", 15) + t("schedules")], ["allocations", ic("link", 15) + t("allocations")],
    ["settings", ic("settings", 15) + t("settings")],
    ["sites", ic("globe", 15) + t("sites")],
    ["watchers", ic("bell", 15) + t("watchers")],
    ["metrics", ic("activity", 15) + t("metrics")],
  ];
  state.page = "server";
  const nav = tabs.map(([tt, label]) => `<a href="#/server/${id}/${tt}" class="${tab === tt ? "active" : ""}">${label}</a>`).join("");
  document.querySelector(".content").innerHTML = `
    <div class="server-head">
      <div>
        <div class="row"><h2 class="server-name">${esc(s.name)}</h2><span class="pill ${statusCls(s.status)}"><i></i>${esc(s.status)}</span></div>
        <div class="server-meta"><span>${esc(s.blueprint)}</span><span>${esc(s.node)}</span>${s.port ? `<button class="allocation-chip" data-act="copyText" data-text="${esc(location.hostname + ":" + s.port)}">${ic("link",12)}${esc(location.hostname)}:${esc(s.port)}${ic("copy",11)}</button>` : `<span class="allocation-chip muted">No endpoint</span>`}</div>
      </div>
      <div class="server-actions">
        ${s.status === "running" ? `
        <button class="btn" aria-label="${t("a_restart")}" data-act="power" data-sid="${id}" data-action="restart">${ic("refresh", 14)}<span>${t("restart")}</span></button>
        <button class="btn danger" aria-label="${t("a_stop")}" data-act="power" data-sid="${id}" data-action="stop">${ic("stop", 14)}<span>${t("stop")}</span></button>
        <button class="icon-btn danger" title="${t("a_kill")}" aria-label="${t("a_kill")}" data-act="confirmKill" data-sid="${id}">${ic("x", 14)}</button>` : `
        <button class="btn success" aria-label="${t("a_start")}" data-act="power" data-sid="${id}" data-action="start">${ic("play", 14)}<span>${t("start")}</span></button>`}
      </div>
    </div>
    <div class="isolation-strip" title="${t("isolation_on")}"><div class="isolation-icon">${ic("shield",18)}</div><span>${t("protected")}</span></div>
    <div class="server-stats">
      <div class="stat"><span class="st-label">${t("status")}</span><span class="st-val" id="st-status">${esc(s.status)}</span></div>
      <div class="stat"><span class="st-label">${t("cpu")}</span><span class="st-val" id="st-cpu">0%</span></div>
      <div class="stat"><span class="st-label">${t("ram")}</span><span class="st-val" id="st-ram">0</span></div>
      <div class="stat"><span class="st-label">Disk</span><span class="st-val" id="st-disk">0</span></div>
      <div class="stat"><span class="st-label">${t("uptime")}</span><span class="st-val" id="st-up">—</span></div>
      <div class="stat"><span class="st-label">PID</span><span class="st-val" id="st-pid">—</span></div>
    </div>
    <div class="tabs">${nav}</div>
    <div id="tab-body"><div class="empty">${ic("server", 40)}<p>${t("loading")}</p></div></div>`;
  state.server = s;
  const render = {
    console: () => renderConsole(id),
    files: () => renderFiles(id),
    settings: () => renderServerSettings(id, data),
    databases: () => renderDatabases(id),
    backups: () => renderBackups(id),
    schedules: () => renderSchedules(id),
    sites: () => renderSites(id),
    watchers: () => renderWatchers(id),
    allocations: () => renderAllocations(id),
    metrics: () => renderMetrics(id),
  };
  (render[tab] || render.console)();
  let busy = false;
  poll(async () => {
    if (token !== routeToken || busy) return; // superseded or tick in flight
    busy = true;
    try {
      const st = await api(`/servers/${id}/stats`);
      if (token !== routeToken) return;
      $("#st-status").textContent = st.status;
      $("#st-status").className = `st-val ${st.status === "running" ? "green" : st.status === "crashed" ? "red" : ""}`;
      $("#st-cpu").textContent = Math.round(st.cpu) + "%";
      $("#st-ram").textContent = fmtBytes(st.memory_bytes) + " / " + st.memory_limit_mb + " MB";
      $("#st-disk").textContent = fmtBytes(st.disk_bytes) + " / " + st.disk_limit_mb + " MB";
      $("#st-up").textContent = fmtTime(st.uptime_secs);
      $("#st-pid").textContent = st.pid || "—";
      pushMetric(id, st);
    } catch (e) {}
    finally { busy = false; }
  }, 3000);
}

async function power(id, action) {
  try { await api(`/servers/${id}/power`, { method: "POST", body: JSON.stringify({ action }) }); toast(`${t(action || "ok")} → ok`, "success"); }
  catch (e) { toast(e.message, "error"); }
}

/* Console follows SSE plain-text lines, not a terminal emulator (no xterm.js):
   ANSI SGR sequences (colors 30-37 / 90-97 / 40-47 / 100-107, bold / dim /
   underline, reset) parse into styled spans built with textContent nodes.
   Anything that is not an `ESC [ params m` sequence renders literally, so a
   malformed or hostile payload degrades to plain text, never to markup. */
const ANSI_RE = /\x1b\[([0-9;]*)m/g;
const sgrApply = (style, code) => {
  if (code === 0) { style.fg = style.bg = null; style.bold = style.dim = style.underline = false; }
  else if (code === 1) style.bold = true;
  else if (code === 2) style.dim = true;
  else if (code === 4) style.underline = true;
  else if (code === 22) { style.bold = style.dim = false; }
  else if (code === 24) style.underline = false;
  else if (code === 39) style.fg = null;
  else if (code === 49) style.bg = null;
  else if (code >= 30 && code <= 37) style.fg = code - 30;
  else if (code >= 40 && code <= 47) style.bg = code - 40;
  else if (code >= 90 && code <= 97) style.fg = code - 90 + 8;
  else if (code >= 100 && code <= 107) style.bg = code - 100 + 8;
  /* unknown SGR codes are ignored — the text still renders */
};
const sgrCls = (style) => {
  const p = [];
  if (style.fg !== null) p.push("fg" + style.fg);
  if (style.bg !== null) p.push("bg" + style.bg);
  if (style.bold) p.push("b");
  if (style.dim) p.push("d");
  if (style.underline) p.push("u");
  return p.length ? "c " + p.join(" ") : "";
};
function ansiLine(doc, raw) {
  const frag = doc.createDocumentFragment();
  const style = { fg: null, bg: null, bold: false, dim: false, underline: false };
  const push = (seg) => {
    if (!seg) return;
    const cls = sgrCls(style);
    if (cls) { const s = doc.createElement("span"); s.className = cls; s.textContent = seg; frag.appendChild(s); }
    else frag.appendChild(doc.createTextNode(seg));
  };
  let last = 0, m;
  ANSI_RE.lastIndex = 0;
  while ((m = ANSI_RE.exec(raw)) !== null) {
    push(raw.slice(last, m.index));
    const params = m[1];
    if (params === "") sgrApply(style, 0);
    else for (const p of params.split(";")) sgrApply(style, parseInt(p, 10));
    last = m.index + m[0].length;
  }
  push(raw.slice(last));
  return frag;
}
const consoleHistory = new Map(); // per-server command history (in-memory)
let consoleHistIdx = -1; // -1 = not browsing history
let consoleFollow = true; // follow-lock: keep the viewport glued to new output
const updateFollowBtn = () => {
  const b = $("#console-follow");
  if (!b) return;
  b.classList.toggle("on", consoleFollow);
  b.title = consoleFollow ? t("console_follow") : t("console_follow_paused");
  b.setAttribute("aria-label", b.title);
};
/* ---------- console ---------- */
function renderConsole(id) {
  state.consoleServer = id;
  $("#tab-body").innerHTML = `<div class="console-wrap">
    <div class="console-bar"><span>${ic("terminal", 14)} ${t("console")} - ${esc(state.server.name)}</span><span class="console-tools">
      <span class="console-status" id="console-status">${t("console_connecting")}</span>
      <button class="icon-btn sm console-follow on" id="console-follow" title="${t("console_follow")}" aria-label="${t("console_follow")}" data-act="consoleFollow">${ic("chevron_right", 14)}</button>
      <button class="icon-btn sm" title="${t("console_clear")}" aria-label="${t("console_clear")}" data-act="consoleClear" data-sid="${id}">${ic("x", 14)}</button>
      <a class="icon-btn sm" title="${t("console_download")}" aria-label="${t("console_download")}" href="/api/servers/${id}/console/log">${ic("download", 14)}</a>
    </span></div>
    <div class="console" id="console-out"></div>
    <div class="console-input">${ic("chevron_right", 16)}<input id="console-cmd" placeholder="${t("console_placeholder")}" autocomplete="off"><button class="btn" data-act="sendCmd" data-sid="${id}">${ic("send", 14)}<span>${t("console_send")}</span></button></div>
  </div>
  <div id="crash-banner" class="card"></div>`;

  const out = $("#console-out");
  consoleFollow = true;
  updateFollowBtn();
  /* Follow-lock: scrolling away from the bottom pauses auto-scroll; scrolling
     back to the tail re-engages it (the button reflects the state). */
  out.addEventListener("scroll", () => {
    const nearBottom = out.scrollTop + out.clientHeight >= out.scrollHeight - 24;
    if (consoleFollow !== nearBottom) { consoleFollow = nearBottom; updateFollowBtn(); }
  });
  const setStatus = (text, cls) => { const pill = $("#console-status"); if (pill) { pill.className = "console-status " + cls; pill.textContent = text; } };
  let retry = 0;
  const open = () => {
    if (state.page !== "server") return;
    const es = new EventSource(`/api/servers/${id}/console/stream`);
    state.consoleEs = es;
    // Runtime lines keep the historical "console" event name.
    es.addEventListener("console", (ev) => { appendConsoleLine(out, ev.data, false); });
    // Install output streams on its own event so the UI can style it apart.
    es.addEventListener("install", (ev) => { appendConsoleLine(out, ev.data, true); });
    // The ring evicted ids this client still needed - note it, keep the tail.
    es.addEventListener("truncated", () => {
      if (!out.childNodes.length) return;
      const n = document.createElement("span");
      n.className = "console-trunc";
      n.textContent = t("console_truncated");
      out.appendChild(n);
      if (consoleFollow) out.scrollTop = out.scrollHeight;
    });
    es.onopen = () => { retry = 0; setStatus(t("console_live"), "ok"); };
    es.onerror = () => {
      es.close();
      if (state.page !== "server") return;
      if (retry >= 5) {
        setStatus(t("console_disconnected"), "bad");
        state.consoleEs = null;
        // Probe the session: api() redirects to login on 401; on any other
        // failure just try the stream again.
        api("/me").then(() => open()).catch(() => {});
        return;
      }
      retry++;
      setStatus(t("console_reconnecting").replace("{n}", retry), "warn");
      setTimeout(open, Math.min(30000, 1000 * Math.pow(2, retry)));
    };
  };
  $("#console-cmd").addEventListener("keydown", (e) => {
    if (e.key === "Enter") { sendCmd(id); return; }
    if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
    const h = consoleHistory.get(id) || [];
    if (!h.length) return;
    e.preventDefault();
    if (e.key === "ArrowUp") {
      consoleHistIdx = consoleHistIdx === -1 ? h.length - 1 : Math.max(0, consoleHistIdx - 1);
    } else {
      if (consoleHistIdx === -1) return;
      consoleHistIdx++;
      if (consoleHistIdx >= h.length) { consoleHistIdx = -1; e.target.value = ""; return; }
    }
    e.target.value = h[consoleHistIdx];
  });
  $("#console-cmd").addEventListener("focus", () => { consoleHistIdx = -1; });
  open();
  renderCrashBanner(id);
}

async function consoleClear(id) {
  try {
    await api(`/servers/${id}/console/clear`, { method: "POST" });
    const out = $("#console-out");
    if (out) out.innerHTML = "";
    consoleFollow = true; // a cleared console starts pinned to the tail again
    updateFollowBtn();
    toast(t("console_cleared"), "success");
  } catch (e) { toast(e.message, "error"); }
}

function appendConsoleLine(out, text, isInstall) {
  const line = document.createElement("span");
  line.className = "line";
  line.appendChild(ansiLine(document, text));
  if (isInstall) line.style.opacity = ".72"; // install lines read as secondary
  out.appendChild(line);
  if (out.childNodes.length > 2000) {
    out.innerHTML = "";
    const n = document.createElement("span");
    n.className = "console-trunc line";
    n.textContent = t("console_output_truncated");
    out.appendChild(n);
  }
  /* Crash-banner history (no console-out id) always pins to the tail; the
     live console honors the follow-lock. */
  if (out.id !== "console-out" || consoleFollow) out.scrollTop = out.scrollHeight;
}

/* Burst counters arrive raw from the node agent. Canonicalize to a
   non-negative integer (fallback 0) so a hostile agent payload cannot
   break out of the "Restarts this burst" text or the value="" attribute
   of the budget input below. */
const crashCount = (v) => { const n = parseInt(v, 10); return Number.isFinite(n) && n >= 0 ? String(n) : "0"; };

/* Crash-state banner + policy editor. Reads GET /api/servers/:id/console/crash;
 * every mutation goes through PATCH /api/servers/:id/console/crash-policy. */
/* Crash-burst incident strip — only when the agent reports live burst data
   (restarts_in_burst ≥ 1). Renders crash → auto-restart → … → final crash with
   a 6-marker cap ("×N more") and the last exit reason on the final node. */
const burstStrip = (st) => {
  const restarts = +crashCount(st.restarts_in_burst);
  if (!(restarts > 0) || !st.burst_since) return "";
  const shown = Math.min(restarts, 6);
  const parts = [];
  for (let i = 1; i <= shown; i++) {
    parts.push(`<span class="pm-chip">${ic("alert", 12)} crash #${i}</span>`);
    parts.push(`<span class="pm-restart" title="auto-restart">${ic("refresh", 12)}</span>`);
  }
  if (restarts > shown) parts.push(`<span class="pm-more">×${restarts - shown} more</span>`);
  parts.push(`<span class="pm-chip pm-final">${ic("alert", 12)} crash #${restarts + 1} <em>${st.reason ? esc(st.reason) : "-"}</em></span>`);
  return `<div class="crash-strip">
    <div class="crash-strip-head">${ic("activity", 12)} <span>Crash burst · since ${esc(fmtDate(st.burst_since))}</span></div>
    <div class="crash-timeline">${parts.join('<span class="pm-arrow">→</span>')}</div>
  </div>`;
};

async function renderCrashBanner(id) {
  let st;
  try { st = (await api(`/servers/${id}/console/crash`)).data; }
  catch (e) { return; } // no ConsoleRead, or the route moved - stay silent
  const banner = $("#crash-banner");
  if (!banner) return;
  const crashed = st.status === "crashed";
  const reason = st.reason ? esc(st.reason) : "-";
  banner.style.display = "block";
  banner.innerHTML = `
    <div class="metric-line">
      <span>${ic("alert", 14)} ${crashed ? "Crashed - restart budget exhausted" : "Crash policy"}</span>
      <span class="badge">${esc(st.status)}${st.auto_restart ? " - auto-restart on" : " - auto-restart off"}</span>
    </div>
    <div class="metric-line"><span>Last exit</span><span>${reason}</span></div>
    <div class="metric-line"><span>Restarts this burst</span><span>${crashCount(st.restarts_in_burst)} / ${crashCount(st.restart_budget)}</span></div>
    ${burstStrip(st)}
    <div class="crash-lines"><details><summary>${ic("terminal", 12)} Last console lines</summary><div class="crash-lines-body"></div></details></div>
    <div class="metric-line">
      <label class="check-row" title="Treat an unrequested clean exit as a crash"><input type="checkbox" id="crash-clean"${st.detect_clean_exit_as_crash ? " checked" : ""}><span class="check-box">${ic("check", 12)}</span><span>clean exit = crash</span></label>
      <label>budget
        <input id="crash-budget" type="number" min="0" max="20" value="${esc(crashCount(st.restart_budget))}">
      </label>
      <button class="btn sm ghost" id="crash-reset" title="Clear the current crash burst">Reset burst</button>
      <button class="btn sm" id="crash-save">Save</button>
    </div>`;
  $("#crash-save").addEventListener("click", async () => {
    const body = {
      detect_clean_exit_as_crash: $("#crash-clean").checked,
      restart_budget: Math.max(0, Math.min(20, parseInt($("#crash-budget").value || "5", 10))),
    };
    try {
      await api(`/servers/${id}/console/crash-policy`, { method: "PATCH", body: JSON.stringify(body) });
      toast(t("t_crash_saved"), "success");
      renderCrashBanner(id);
    } catch (e) { toast(e.message, "error"); }
  });
  $("#crash-reset").addEventListener("click", async () => {
    try {
      await api(`/servers/${id}/console/crash-policy`, { method: "PATCH", body: JSON.stringify({ reset_burst: true }) });
      toast(t("t_crash_reset"), "success");
      renderCrashBanner(id);
    } catch (e) { toast(e.message, "error"); }
  });
  const linesDetails = $(".crash-lines details");
  if (linesDetails) {
    linesDetails.addEventListener("toggle", () => {
      if (!linesDetails.open || linesDetails.dataset.loaded) return;
      linesDetails.dataset.loaded = "1";
      const body = linesDetails.querySelector(".crash-lines-body");
      api(`/servers/${id}/console`).then((r) => {
        const lines = r.data || [];
        for (const ln of lines) appendConsoleLine(body, typeof ln === "string" ? ln : (ln && ln.text) || "", false);
        if (!lines.length) body.textContent = "no buffered output";
      }).catch((e) => { body.textContent = "could not load history: " + e.message; });
    });
  }
}

async function confirmKill(id) {
  if (await vpConfirm("Force-kill this process group? Unsaved data may be lost.", "Force kill server")) power(id, "kill");
}

async function sendCmd(id) {
  const inp = $("#console-cmd");
  if (!inp.value.trim()) return;
  const cmd = inp.value;
  try { await api(`/servers/${id}/console/command`, { method: "POST", body: JSON.stringify({ command: cmd }) }); }
  catch (e) { toast(e.message, "error"); }
  inp.value = "";
  const h = consoleHistory.get(id) || [];
  if (h[h.length - 1] !== cmd) h.push(cmd);
  if (h.length > 200) h.shift();
  consoleHistory.set(id, h);
  consoleHistIdx = -1;
}

/* ---------- files ---------- */
let fileSort = { key: "name", dir: 1 };
let fileSel = new Set(); // selected paths (bulk actions)
let fileClip = null; // { paths: [...], from: dir } — cut/paste (moves)
let filesToken = 0; // sequence token: a slow earlier listing must not clobber a newer one

async function renderFiles(id) {
  state.fileId = id;
  fileSel = new Set();
  fileClip = null;
  $("#tab-body").innerHTML = `<div class="files-toolbar">
    <div class="crumbs" id="f-crumbs"></div>
    <div class="f-search">${ic("search", 14)}<input id="f-filter" placeholder="${t("filter_ph")}" aria-label="${t("filter_files")}"></div>
    <button class="icon-btn" title="${t("upload")}" aria-label="${t("upload")}" data-act="fileUpload" data-sid="${id}">${ic("upload", 16)}</button>
    <button class="icon-btn" title="${t("a_pull_url")}" aria-label="${t("a_pull_url")}" data-act="filePull" data-sid="${id}">${ic("link", 16)}</button>
    <button class="icon-btn" title="${t("a_new_file")}" aria-label="${t("a_new_file")}" data-act="fileNewFile" data-sid="${id}">${ic("file", 16)}</button>
    <button class="icon-btn" title="${t("a_new_folder")}" aria-label="${t("a_new_folder")}" data-act="fileNewDir" data-sid="${id}">${ic("folder", 16)}</button>
    <button class="icon-btn" title="${t("a_refresh")}" aria-label="${t("a_refresh")}" data-act="fileOpenDir" data-sid="${id}" data-path="${esc(state.filePath)}">${ic("refresh_ccw", 16)}</button>
  </div>
  <div class="file-dropzone" id="file-drop">
    <div class="f-bulkbar" id="f-bulkbar" hidden>
      <span class="muted" id="f-selcount"></span>
      <button class="btn xs ghost" data-act="fileCutSel" data-sid="${id}">${ic("copy", 12)}<span>${t("cut")}</span></button>
      <button class="btn xs ghost" data-act="fileZipSel" data-sid="${id}">${ic("archive", 12)}<span>${t("archive")}</span></button>
      <button class="btn xs ghost" data-act="fileDlSel" data-sid="${id}">${ic("download", 12)}<span>${t("download")}</span></button>
      <button class="btn xs ghost danger" data-act="fileDelSel" data-sid="${id}">${ic("trash", 12)}<span>${t("delete")}</span></button>
      <button class="btn xs ghost" data-act="fileSelNone">${ic("x", 12)}<span>${t("clear")}</span></button>
      <span class="spacer"></span>
      <button class="btn xs primary" data-act="filePaste" data-sid="${id}" hidden>${ic("check", 12)}<span>${t("paste_here")}</span></button>
    </div>
    <div class="f-uploadbar" id="f-uploadbar" hidden>
      <span class="muted" id="f-upname"></span>
      <div class="progress"><div id="f-upbar"></div></div>
    </div>
    <div id="file-list"><div class="file-list">${'<div class="skeleton sk-file"></div>'.repeat(5)}</div></div>
  </div>
  <input type="file" id="file-picker" multiple>`;
  $("#f-filter").addEventListener("input", () => renderFileRows(id));
  wireDropzone(id);
  await loadFiles(id, "/");
}

async function loadFiles(id, path) {
  const token = ++filesToken;
  state.filePath = path;
  const parts = path.split("/").filter(Boolean);
  let crumb = `<a href="#" data-act="fileOpenDir" data-sid="${id}" data-path="/">/</a>`;
  let acc = "";
  parts.forEach((p, i) => {
    acc += "/" + p;
    crumb += `<span class="crumb-sep">/</span><a href="#" data-act="fileOpenDir" data-sid="${id}" data-path="${esc(acc)}">${esc(p)}</a>`;
  });
  $("#f-crumbs").innerHTML = crumb;
  const list = $("#file-list");
  if (list) list.innerHTML = `<div class="file-list">${'<div class="skeleton sk-file"></div>'.repeat(5)}</div>`;
  try {
    const res = await api(`/servers/${id}/files?path=${encodeURIComponent(path)}`);
    if (token !== filesToken) return; // a newer listing superseded us
    state.fileEntries = res.data || [];
    renderFileRows(id);
  } catch (e) {
    if (token !== filesToken) return;
    list.innerHTML = `<div class="file-list"><div class="file-error">
      ${ic("alert", 26)}<div><b>${t("err_load_files")}</b><span>${esc(e.message)}</span></div>
      <button class="btn sm" data-act="fileOpenDir" data-sid="${id}" data-path="${esc(path)}">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button>
    </div></div>`;
  }
}

const sortArrow = (key) => fileSort.key === key ? (fileSort.dir > 0 ? " ↑" : " ↓") : "";

function renderFileRows(id) {
  const list = $("#file-list");
  if (!list) return;
  const q = ($("#f-filter")?.value || "").trim().toLowerCase();
  let entries = (state.fileEntries || []).slice();
  if (q) entries = entries.filter((f) => f.name.toLowerCase().includes(q));
  const key = fileSort.key, dir = fileSort.dir;
  entries.sort((a, b) => {
    if (key === "size") return dir * ((a.size || 0) - (b.size || 0));
    if (key === "date") return dir * ((+a.modified || 0) - (+b.modified || 0));
    return dir * a.name.localeCompare(b.name);
  });
  const row = (f) => `<div class="file-row${fileSel.has(f.path) ? " sel" : ""}">
    <label class="f-check"><input type="checkbox" data-act-change="fileToggleSel" data-path="${esc(f.path)}"${fileSel.has(f.path) ? " checked" : ""}><span></span></label>
    <span class="f-icon">${fileIcon(f.extension, f.is_dir)}</span>
    <span class="f-name" data-act="${f.is_dir ? "fileOpenDir" : "fileOpen"}" data-sid="${id}" data-path="${esc(f.path)}" role="link" tabindex="0">${esc(f.name)}</span>
    <span class="f-meta size">${f.is_dir ? "—" : fmtBytes(f.size)}</span>
    <span class="f-meta date">${fmtFileDate(f.modified)}</span>
    <span class="f-meta type">${esc((f.mime || "file").split("/")[0])}</span>
    <span class="f-actions">
      <button class="icon-btn sm" title="${f.is_dir ? t("download") + " zip" : t("download")}" aria-label="${f.is_dir ? "Download folder as zip" : t("download")}" data-act="fileDl" data-sid="${id}" data-path="${esc(f.path)}">${ic("download", 15)}</button>
      <button class="icon-btn sm" title="Rename" aria-label="Rename" data-act="fileRename" data-sid="${id}" data-path="${esc(f.path)}">${ic("pencil", 15)}</button>
      <button class="icon-btn sm" title="More actions" aria-label="More actions" data-act="fileMenu" data-sid="${id}" data-path="${esc(f.path)}" data-dir="${f.is_dir ? "1" : "0"}" data-ext="${esc(f.extension)}">${ic("more", 15)}</button>
    </span>
  </div>`;
  list.innerHTML = entries.length ? `<div class="file-list">
    <div class="file-row head">
      <span></span><span></span>
      <button class="sort-btn" data-act="fileSort" data-key="name">${t("name")}${sortArrow("name")}</button>
      <button class="sort-btn" data-act="fileSort" data-key="size">${t("size")}${sortArrow("size")}</button>
      <button class="sort-btn" data-act="fileSort" data-key="date">${t("modified")}${sortArrow("date")}</button>
      <span>${t("type")}</span><span>${t("actions")}</span>
    </div>
    ${entries.map(row).join("")}
  </div>` : `<div class="file-list">${q ? emptyState("search", t("es_search_none_t"), "") : emptyState("folder", t("es_files_none_t"), "")}</div>`;
  const bar = $("#f-bulkbar");
  if (bar) {
    const n = fileSel.size;
    bar.hidden = !n && !fileClip;
    $("#f-selcount").textContent = n ? `${n} selected` : fileClip ? `${fileClip.paths.length} cut` : "";
    const paste = bar.querySelector('[data-act="filePaste"]');
    if (paste) { paste.hidden = !fileClip; paste.querySelector("span").textContent = fileClip ? `Paste ${fileClip.paths.length} here` : "Paste here"; }
  }
}

function fileSortToggle(key) {
  if (fileSort.key === key) fileSort.dir *= -1;
  else { fileSort.key = key; fileSort.dir = 1; }
  renderFileRows(state.fileId);
}
function fileToggleSel(cb) {
  if (cb.checked) fileSel.add(cb.dataset.path);
  else fileSel.delete(cb.dataset.path);
  renderFileRows(state.fileId);
}
function fileSelNone() { fileSel = new Set(); renderFileRows(state.fileId); }

/* Row "more" menu — absolute dropdown owned by the row. */
function closeFileMenus() { $$(".file-menu").forEach((m) => m.remove()); }
function fileMenu(id, path, isDir, ext, btn) {
  closeFileMenus();
  const item = (label, act, danger) => `<button class="file-menu-item${danger ? " danger" : ""}" data-act="${act}" data-sid="${id}" data-path="${esc(path)}"><span>${label}</span></button>`;
  let items = item("Rename", "fileRename") + item("Copy", "fileCopy") + item("Move…", "fileMoveAsk") + item("Permissions…", "fileChmod");
  const zippy = /^(zip|gz|tar|tgz|7z|rar)$/i.test(ext || "");
  if (!zippy) items += item(t("archive"), "fileZip");
  if (!isDir && zippy) items += item(t("extract"), "fileExtract");
  items += item("Delete", "fileDel", true);
  const menu = document.createElement("div");
  menu.className = "file-menu";
  menu.innerHTML = items;
  btn.parentElement.appendChild(menu);
}

async function fileMoveAsk(id, path) {
  const dest = await vpPrompt("Move to directory:", state.filePath);
  if (!dest) return;
  try { await api(`/servers/${id}/files/move`, { method: "POST", body: JSON.stringify({ files: [path], dest }) }); toast(t("moved"), "success"); await loadFiles(id, state.filePath); }
  catch (e) { toast(e.message, "error"); }
}
function fileCutSel(id) {
  if (!fileSel.size) return;
  fileClip = { paths: [...fileSel], from: state.filePath };
  toast(`${fileClip.paths.length} item(s) cut — paste into a folder`, "info");
  renderFileRows(id);
}
async function filePaste(id) {
  if (!fileClip) return;
  try {
    await api(`/servers/${id}/files/move`, { method: "POST", body: JSON.stringify({ files: fileClip.paths, dest: state.filePath }) });
    toast(t("moved"), "success");
    fileClip = null; fileSel = new Set();
    await loadFiles(id, state.filePath);
  } catch (e) { toast(e.message, "error"); }
}
async function fileDelSel(id) {
  if (!fileSel.size) return;
  if (!await vpConfirm(`Delete ${fileSel.size} selected item(s)?`)) return;
  try {
    for (const p of fileSel) await api(`/servers/${id}/files/delete`, { method: "POST", body: JSON.stringify({ path: p }) });
    fileSel = new Set(); fileClip = null;
    toast(t("deleted"), "success");
    await loadFiles(id, state.filePath);
  } catch (e) { toast(e.message, "error"); }
}

async function fileZipSel(id) {
  if (!fileSel.size) return;
  try {
    const res = await api(`/servers/${id}/files/archive`, { method: "POST", body: JSON.stringify({ path: "", paths: [...fileSel], format: "zip" }) });
    fileSel = new Set();
    toast(`Created ${res.path || "archive"}`, "success");
    await loadFiles(id, state.filePath);
  } catch (e) { toast(e.message, "error"); }
}

function fileDlSel(id) {
  if (!fileSel.size) return;
  const qs = [...fileSel].map((p) => `paths=${encodeURIComponent(p)}`).join("&");
  window.location.href = `/api/servers/${id}/files/download_multi?${qs}`;
}
async function fileChmod(id, path) {
  const cur = (state.fileEntries || []).find((f) => f.path === path);
  const prefill = cur && typeof cur.mode === "number" ? (cur.mode & 0o777).toString(8) : "755";
  const mode = await vpPrompt("Permissions (octal, e.g. 755):", prefill);
  if (!mode) return;
  try { await api(`/servers/${id}/files/chmod`, { method: "POST", body: JSON.stringify({ path, mode }) }); toast(t("t_perms_updated"), "success"); await loadFiles(id, state.filePath); }
  catch (e) { toast(e.message, "error"); }
}
async function fileZip(id, path) {
  try {
    const res = await api(`/servers/${id}/files/archive`, { method: "POST", body: JSON.stringify({ path, format: "zip" }) });
    toast(`Created ${res.path || "archive"}`, "success");
    await loadFiles(id, state.filePath);
  } catch (e) { toast(e.message, "error"); }
}
async function fileExtract(id, path) {
  try {
    await api(`/servers/${id}/files/extract`, { method: "POST", body: JSON.stringify({ archive: path, dest: state.filePath }) });
    toast(t("extracted"), "success");
    await loadFiles(id, state.filePath);
  } catch (e) { toast(e.message, "error"); }
}

async function fileOpen(id, path) {
  let res;
  try { res = await api(`/servers/${id}/files/read?path=${encodeURIComponent(path)}`); }
  catch (e) { toast(e.message, "error"); return; }
  const content = res.content_b64 ? atob(res.content_b64) : "";
  const isImage = /^image\//.test(res.mime || "");
  const name = path.split("/").pop();
  const modal = document.createElement("div");
  modal.className = "modal";
  /* Image previews load from a blob: URL, never an inline data: URL. Blob URLs
     are opaque-origin, so an SVG preview cannot trigger cookie-bearing
     subresource loads from the panel's origin. The URL is revoked when the
     modal leaves the DOM (see modalObserver). */
  let previewUrl = null;
  if (isImage && res.content_b64) {
    const bin = atob(res.content_b64);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    previewUrl = URL.createObjectURL(new Blob([bytes], { type: res.mime || "application/octet-stream" }));
    modal.dataset.previewUrl = previewUrl;
  }
  modal.innerHTML = `<div class="modal-card big file-editor">
    <div class="modal-head"><b>${fileIcon((res.mime || "").includes("json") ? "json" : "txt", false)} <span class="modal-title">${esc(name)}</span></b><div class="row"><span class="badge">${esc(res.mime)}</span><span class="badge warn" id="editor-dirty" hidden>unsaved</span><button class="icon-btn" data-act="closeModal" aria-label="Close">${ic("x", 16)}</button></div></div>
    ${isImage ? `<div class="file-preview"><img src="${previewUrl || ""}" alt="${esc(name)}"></div>` : `<textarea id="editor" spellcheck="false">${esc(content)}</textarea>`}
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button>${isImage ? "" : `<button class="btn primary" data-act="fileSave" data-sid="${id}" data-path="${esc(path)}">${ic("save", 14)}<span>${t("save")}</span></button>`}</div>
  </div>`;
  const ta = $("#editor");
  if (ta) {
    ta.focus();
    ta.addEventListener("input", () => { if (ta.value !== content) ta.dataset.dirty = "1"; else delete ta.dataset.dirty; const d = $("#editor-dirty"); if (d) d.hidden = ta.value === content; });
    ta.addEventListener("keydown", (e) => { if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") { e.preventDefault(); fileSave(id, path); } });
    const guard = async (ev) => {
      ev.preventDefault(); ev.stopPropagation();
      if (ta.dataset.dirty && !await vpConfirm("Discard unsaved changes?", "Unsaved changes")) return;
      modal.remove();
    };
    modal.querySelectorAll('[data-act="closeModal"]').forEach((b) => b.addEventListener("click", guard));
    // Escape bubbles to the document close handler — intercept it here so the
    // dirty prompt applies to keyboard close too.
    modal.addEventListener("keydown", (e) => {
      if (e.key !== "Escape" || !ta.dataset.dirty) return;
      e.stopPropagation();
      (async () => { if (await vpConfirm("Discard unsaved changes?", "Unsaved changes")) modal.remove(); })();
    });
  }
}

async function fileSave(id, path, btn) {
  const ta = $("#editor");
  if (!ta) return;
  const content = ta.value;
  try {
    await api(`/servers/${id}/files/write`, { method: "POST", body: JSON.stringify({ path, content }) });
    toast(t("saved"), "success");
    delete ta.dataset.dirty;
    const d = $("#editor-dirty"); if (d) d.hidden = true;
    const m = btn ? btn.closest(".modal") : null;
    (m || $(".modal"))?.remove();
  } catch (e) { toast(e.message, "error"); }
}
async function fileDl(id, path) { window.location = `/api/servers/${id}/files/download?path=${encodeURIComponent(path)}`; }
async function fileDel(id, path) { if (!await vpConfirm(`${t("confirm_delete")} ${path}`)) return; try { await api(`/servers/${id}/files/delete`, { method: "POST", body: JSON.stringify({ path }) }); toast(t("deleted"), "success"); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }
async function fileRename(id, path) { const name = await vpPrompt("New name:", path.split("/").pop()); if (!name) return; const to = path.slice(0, path.lastIndexOf("/")) + "/" + name; try { await api(`/servers/${id}/files/rename`, { method: "POST", body: JSON.stringify({ from: path, to }) }); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }
async function fileCopy(id, path) { const to = path + ".copy"; try { await api(`/servers/${id}/files/copy`, { method: "POST", body: JSON.stringify({ from: path, to }) }); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }
async function fileNewFile(id) { const name = await vpPrompt("File name:"); if (!name) return; try { await api(`/servers/${id}/files/touch`, { method: "POST", body: JSON.stringify({ path: state.filePath + "/" + name }) }); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }
async function fileNewDir(id) { const name = await vpPrompt("Directory name:"); if (!name) return; try { await api(`/servers/${id}/files/mkdir`, { method: "POST", body: JSON.stringify({ path: state.filePath + "/" + name }) }); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }

/* XHR upload with per-file progress; sequential so one progress bar is honest. */
function uploadFiles(id, files) {
  if (!files.length) return;
  const bar = $("#f-uploadbar"), pbar = $("#f-upbar"), name = $("#f-upname");
  if (!bar || !pbar) return;
  bar.hidden = false;
  const queue = [...files];
  const next = () => {
    if (!queue.length) {
      bar.hidden = true;
      pbar.style.width = "0";
      toast(t("uploaded"), "success");
      loadFiles(id, state.filePath);
      return;
    }
    const f = queue.shift();
    name.textContent = `Uploading ${f.name}`;
    const fd = new FormData();
    fd.append("path", state.filePath);
    fd.append("file", f, f.name);
    const xhr = new XMLHttpRequest();
    xhr.open("POST", `/api/servers/${id}/files/upload`);
    xhr.upload.onprogress = (ev) => { if (ev.lengthComputable) pbar.style.width = Math.round((ev.loaded / ev.total) * 100) + "%"; };
    xhr.onload = () => {
      if (xhr.status >= 400) {
        let m = "upload failed";
        try { m = JSON.parse(xhr.responseText).error || m; } catch (_) {}
        toast(m, "error");
      }
      next();
    };
    xhr.onerror = () => { toast(t("t_upload_failed"), "error"); next(); };
    xhr.send(fd);
  };
  next();
}
function fileUpload(id) { const picker = $("#file-picker"); picker.onchange = () => { uploadFiles(id, [...picker.files]); picker.value = ""; }; picker.click(); }
function wireDropzone(id) {
  const zone = $("#file-drop");
  if (!zone) return;
  let depth = 0;
  zone.addEventListener("dragenter", (e) => { e.preventDefault(); depth++; zone.classList.add("drag"); });
  zone.addEventListener("dragover", (e) => e.preventDefault());
  zone.addEventListener("dragleave", () => { if (--depth <= 0) { depth = 0; zone.classList.remove("drag"); } });
  zone.addEventListener("drop", (e) => {
    e.preventDefault(); depth = 0; zone.classList.remove("drag");
    const files = [...(e.dataTransfer?.files || [])];
    if (files.length) uploadFiles(id, files);
  });
}

/* ---------- server settings ---------- */
function renderServerSettings(id, data) {
  const s = data.server;
  const varInput = (v) => {
    const k = v.kind || { type: "text" };
    const dis = v.user_editable ? "" : "disabled";
    const val = esc(v.value);
    if (k.type === "choice") {
      const opts = (k.options || []).map((o) => `<option value="${esc(o)}"${o === v.value ? " selected" : ""}>${esc(o)}</option>`).join("");
      return `<select data-var="${esc(v.env_var)}" ${dis}>${opts}</select>`;
    }
    if (k.type === "bool") {
      const on = String(v.value).toLowerCase() === "true";
      return `<label class="check-row"><input type="checkbox" data-var="${esc(v.env_var)}" data-bool="1"${on ? " checked" : ""} ${dis}><span class="check-box">${ic("check", 13, 2.4)}</span></label>`;
    }
    const attrs = [];
    let type = "text";
    if (k.type === "number") {
      type = "number";
      if (k.min !== undefined && k.min !== null) attrs.push(`min="${esc(String(k.min))}"`);
      if (k.max !== undefined && k.max !== null) attrs.push(`max="${esc(String(k.max))}"`);
    } else if (k.type === "url") {
      type = "url";
    }
    if (k.max_len) attrs.push(`maxlength="${esc(String(k.max_len))}"`);
    if (k.pattern) attrs.push(`pattern="${esc(k.pattern)}"`);
    if (v.required) attrs.push("required");
    return `<div class="field-input">${ic("pencil", 14)}<input type="${type}" data-var="${esc(v.env_var)}" value="${val}" ${attrs.join(" ")} ${dis}></div>`;
  };
  const kindLabel = (k) => {
    if (!k || k.type === "text") return "";
    return `<span class="badge">${esc(k.type)}</span>`;
  };
  const vars = (data.variables || []).map((v) => `<div class="field">
    <label>${esc(v.name)} <code>${esc(v.env_var)}</code> ${kindLabel(v.kind)}${v.required ? '<span class="badge">required</span>' : ""}${v.user_editable ? "" : '<span class="badge">locked</span>'}</label>
    ${varInput(v)}
    <small>${esc(v.description)}</small>
  </div>`).join("");
  const subUsers = (data.subusers || []).map((su) => `<div class="file-row"><span class="avatar mini">${esc(su.username[0] || "?").toUpperCase()}</span><b>${esc(su.username)}</b><span class="badge">${esc(su.role || "custom")}</span><span class="f-meta">${(su.permissions || []).join(", ") || t("none")}</span><span class="f-actions"><button class="icon-btn sm danger" data-act="subDel" data-sid="${id}" data-sub="${su.id}">${ic("trash", 15)}</button></span></div>`).join("");
  $("#tab-body").innerHTML = `
    <div class="grid cols-2">
      <div class="card"><h3>${ic("terminal", 15)} Launch Plan</h3><div class="code-block">${esc(data.resolved_launch)}</div>
        <div class="field"><label>Runtime hint</label><div class="field-input">${ic("box", 14)}<input id="s-runtime" value="${esc(data.runtime_hint)}" ${state.user.root_admin ? "" : "disabled"}></div></div>
        <div class="row"><button class="btn sm" data-act="saveRuntime" data-sid="${id}">${ic("save", 13)}<span>${t("save")}</span></button></div>
      </div>
      <div class="card"><h3>${ic("settings", 15)} ${t("settings")}</h3>
        <div class="metric-line"><span>Auto-restart on crash</span><label class="switch"><input type="checkbox" id="s-autorestart" ${s.auto_restart ? "checked" : ""} data-act="saveToggle" data-act-change data-sid="${id}" data-field="auto_restart" data-val="${s.auto_restart ? "0" : "1"}"><i></i></label></div>
        <div class="metric-line"><span>Memory</span><b>${s.memory_mb} MB</b></div>
        <div class="metric-line"><span>Disk</span><b>${s.disk_mb} MB</b></div>
        <div class="metric-line"><span>CPU limit</span><b>${s.cpu_percent}%</b></div>
        <div class="metric-line"><span>Restart count</span><b>${s.restart_count}</b></div>
        <div class="metric-line"><span>Execution agent</span><b>${esc(s.node)}</b></div>
      </div>
    </div>
    <div class="card"><h3>${ic("sliders", 15)} Variables</h3>${vars || `<div class="muted">${t("none")}</div>`}<button class="btn primary" data-act="saveVars" data-sid="${id}">${ic("save", 14)}<span>${t("save")} Variables</span></button></div>
    <div class="grid cols-2">
      <div class="card"><h3>${ic("users", 15)} Subusers</h3>${subUsers || `<div class="muted">${t("none")}</div>`}
        <div class="row mt-10"><input id="sub-user-id" placeholder="user id"><select id="sub-role">${["viewer", "operator", "developer", "manager"].map((r) => `<option value="${r}"${r === "operator" ? " selected" : ""}>${r}</option>`).join("")}</select><button class="btn sm" data-act="subAdd" data-sid="${id}">${ic("plus", 13)}<span>${t("create")}</span></button></div>
      </div>
      <div class="card"><h3>${ic("alert", 15)} Danger Zone</h3>
        <div class="row danger-row">
          <button class="btn" data-act="install" data-sid="${id}">${ic("refresh", 13)}<span>${t("reinstall")}</span></button>
          <button class="btn" data-act="suspend" data-sid="${id}">${ic("pause", 13)}<span>${t("suspend")}</span></button>
          <button class="btn danger" data-act="delServer" data-sid="${id}">${ic("trash", 13)}<span>${t("delete")}</span></button>
        </div>
      </div>
    </div>`;
}

async function saveRuntime(id) { const runtime_hint = $("#s-runtime").value; try { await api(`/servers/${id}`, { method: "PATCH", body: JSON.stringify({ runtime_hint }) }); toast(t("saved"), "success"); } catch (e) { toast(e.message, "error"); } }
async function saveToggle(id, field, val) { try { await api(`/servers/${id}`, { method: "PATCH", body: JSON.stringify({ [field]: val }) }); toast(t("saved"), "success"); } catch (e) { toast(e.message, "error"); } }
async function saveVars(id) {
  const variables = {};
  $$("[data-var]").forEach((inp) => { variables[inp.dataset.var] = inp.dataset.bool ? String(inp.checked) : inp.value; });
  try { await api(`/servers/${id}/variables`, { method: "POST", body: JSON.stringify({ variables }) }); toast(t("saved"), "success"); }
  catch (e) { toast(e.message, "error"); }
}
/* Re-render the settings tab in place (no full SPA reload) so a one-row
   subuser change updates just the affected view. */
async function reloadSettings(id) {
  try { const data = await api(`/servers/${id}`); renderServerSettings(id, data); }
  catch (e) { toast(e.message, "error"); }
}
async function subAdd(id) { const user_id = +$("#sub-user-id").value; if (!user_id) return; const role = $("#sub-role").value; try { await api(`/servers/${id}/subusers`, { method: "POST", body: JSON.stringify({ user_id, role }) }); toast(t("created"), "success"); await reloadSettings(id); } catch (e) { toast(e.message, "error"); } }
async function subDel(id, sub_id) { if (!await vpConfirm(t("confirm_delete"))) return; try { await api(`/servers/${id}/subusers/${sub_id}`, { method: "DELETE" }); await reloadSettings(id); } catch (e) { toast(e.message, "error"); } }
async function install(id) { try { await api(`/servers/${id}/install`, { method: "POST", body: JSON.stringify({}) }); toast(t("t_install_queued"), "success"); } catch (e) { toast(e.message, "error"); } }
async function suspend(id) { try { await api(`/servers/${id}/suspend`, { method: "POST" }); toast(t("saved"), "success"); } catch (e) { toast(e.message, "error"); } }
async function delServer(id) {
  const name = state.server?.name || `server-${id}`;
  if (!await vpDestroy({
    kind: "server", target: name,
    consequences: ["All Files, Databases and Backups data are erased", "Allocated ports are released", "This cannot be undone"],
  })) return;
  try { await api(`/servers/${id}`, { method: "DELETE" }); location.hash = "#/"; } catch (e) { toast(e.message, "error"); }
}

/* ---------- databases ---------- */
async function renderDatabases(id) {
  if (state.server?.node !== "local") {
    $("#tab-body").innerHTML = `<div class="empty">${ic("database", 40)}<p>Databases are available on local servers only.</p></div>`;
    return;
  }
  $("#tab-body").innerHTML = `<div class="card"><h3>${ic("database", 15)} ${t("databases")} <span class="badge">SQLite</span></h3>
    <div id="db-list"><div class="empty">${ic("database", 40)}<p>${t("loading")}</p></div></div>
    <div class="field-row mt-12"><input id="db-name" placeholder="db name"><button class="btn primary" data-act="dbCreate" data-sid="${id}">${ic("plus", 14)}<span>${t("create")}</span></button></div>
  </div>`;
  await dbLoad(id);
}
async function dbLoad(id) {
  const box = $("#db-list");
  try {
    const res = await api(`/servers/${id}/databases`);
    const dbs = res.data || [];
    box.innerHTML = dbs.length ? `<div class="file-list">${dbs.map((d) => `<div class="file-row"><span class="f-icon">${ic("database", 16)}</span><div class="db-main"><b>${esc(d.name)}</b><div class="f-meta">${fmtBytes(d.size)}</div></div><span class="f-actions"><button class="icon-btn sm" title="Open Database" aria-label="Open Database" data-act="dbOpen" data-sid="${id}" data-name="${esc(d.name)}">${ic("terminal", 15)}</button><button class="icon-btn sm danger" title="${t("delete")}" aria-label="${t("delete")}" data-act="dbDrop" data-sid="${id}" data-name="${esc(d.name)}">${ic("trash", 15)}</button></span></div>`).join("")}</div>` : emptyState("database", t("es_db_t"), t("es_db_h"));
  } catch (e) {
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_databases")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="dbRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
async function dbCreate(id) { const name = $("#db-name").value.trim(); if (!name) { toast(t("e_required_name"), "warn"); return; } try { await api(`/servers/${id}/databases`, { method: "POST", body: JSON.stringify({ name }) }); toast(t("created"), "success"); await dbLoad(id); } catch (e) { toast(e.message, "error"); } }
async function dbDrop(id, name) { if (!await vpConfirm(`${t("confirm_delete")} ${name}?`)) return; try { await api(`/servers/${id}/databases/${encodeURIComponent(name)}`, { method: "DELETE" }); await dbLoad(id); } catch (e) { toast(e.message, "error"); } }

/* Data Lab: schema tree (tables + columns) beside a read/write SQL runner. */
async function dbOpen(id, name) {
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.innerHTML = `<div class="modal-card big db-modal">
    <div class="modal-head"><b>${ic("database", 16)} <span class="modal-title">${esc(name)}</span></b><button class="icon-btn" data-act="closeModal" aria-label="Close">${ic("x", 16)}</button></div>
    <div class="db-body">
      <div class="db-schema">
        <div class="db-pane-head"><b>Schema</b><button class="icon-btn sm" title="Refresh schema" aria-label="Refresh schema" data-act="dbSchema" data-sid="${id}" data-name="${esc(name)}">${ic("refresh_ccw", 13)}</button></div>
        <div id="db-schema-tree"><div class="skeleton sk-db"></div></div>
      </div>
      <div class="db-main">
        <label class="check-row db-write-toggle" title="INSERT/UPDATE/DELETE/DDL statements run against /exec; everything else stays read-only">
          <input type="checkbox" id="db-write"><span class="check-box">${ic("check", 12)}</span><span>Write mode</span>
        </label>
        <textarea id="db-sql" placeholder="SELECT * FROM sqlite_master;" spellcheck="false">SELECT * FROM sqlite_master WHERE type='table';</textarea>
        <div class="row"><button class="btn primary" data-act="dbExec" data-sid="${id}" data-name="${esc(name)}">${ic("play", 13)}<span>Run</span></button></div>
        <div id="db-out" class="db-out"></div>
      </div>
    </div>
  </div>`;
  document.body.appendChild(modal);
  const writeBox = $("#db-write");
  writeBox?.addEventListener("change", async (e) => {
    if (e.target.checked && !await vpConfirm("Write mode executes statements against the server database — confirm", "Enable write mode")) e.target.checked = false;
  });
  dbSchema(id, name);
}
async function dbSchema(id, name) {
  const box = $("#db-schema-tree");
  if (!box) return;
  box.innerHTML = `<div class="skeleton sk-db"></div>`;
  try {
    const tables = await api(`/servers/${id}/databases/${encodeURIComponent(name)}/tables`);
    const arr = Array.isArray(tables) ? tables : (tables.data || []);
    if (!arr.length) { box.innerHTML = `<div class="muted">No tables</div>`; return; }
    box.innerHTML = arr.map((tb, i) => `<div class="db-table">
      <button class="db-table-head" data-act="dbSchemaCols" data-sid="${id}" data-name="${esc(name)}" data-table="${esc(tb.name || tb.Name || tb)}" data-idx="${i}">${ic("box", 13)}<span>${esc(tb.name || tb.Name || tb)}</span>${ic("chevron_right", 12)}</button>
      <div class="db-cols" id="db-cols-${i}" hidden></div>
    </div>`).join("");
  } catch (e) { box.innerHTML = `<div class="muted">${esc(e.message)}</div>`; }
}
async function dbSchemaCols(id, name, table, idx) {
  const box = document.getElementById("db-cols-" + idx);
  if (!box) return;
  if (!box.hidden) { box.hidden = true; return; }
  $$(".db-cols").forEach((c) => { c.hidden = true; });
  box.hidden = false;
  box.innerHTML = `<div class="muted">loading…</div>`;
  try {
    const cols = await api(`/servers/${id}/databases/${encodeURIComponent(name)}/query`, { method: "POST", body: JSON.stringify({ sql: `PRAGMA table_info('${table.replace(/'/g, "''")}')` }) });
    const arr = Array.isArray(cols) ? cols : (cols.data || []);
    box.innerHTML = arr.length ? `<div class="db-cols-list">${arr.map((c) => `<div class="db-col"><code>${esc(c.name)}</code><span>${esc(c.type || "")}</span>${c.pk ? `<b>PK</b>` : ""}${c.notnull ? `<b>NN</b>` : ""}</div>`).join("")}</div>` : `<div class="muted">no columns</div>`;
  } catch (e) { box.innerHTML = `<div class="muted">${esc(e.message)}</div>`; }
}
async function dbExec(id, name) {
  const sql = $("#db-sql").value;
  const write = $("#db-write")?.checked;
  const out = $("#db-out");
  if (!out) return;
  out.textContent = "Running…";
  try {
    const res = await api(`/servers/${id}/databases/${encodeURIComponent(name)}/${write ? "exec" : "query"}`, { method: "POST", body: JSON.stringify({ sql }) });
    if (write) {
      out.textContent = JSON.stringify(res, null, 2);
      dbSchema(id, name); // schema may have changed — refresh the tree
    } else {
      renderDbResults(out, res);
    }
  } catch (e) { out.innerHTML = `<div class="file-error">${ic("alert", 18)}<div><b>${write ? t("err_write_blocked") : t("err_query_failed")}</b><span>${esc(e.message)}</span></div></div>`; }
}
/* Render query rows as a real table (headers from the first row), never a JSON blob. */
function renderDbResults(out, res) {
  const rows = Array.isArray(res) ? res : (res?.data || []);
  if (!Array.isArray(rows) || !rows.length) { out.textContent = rows && rows.length === 0 ? "0 rows" : JSON.stringify(res, null, 2); return; }
  const cols = Object.keys(rows[0]);
  const cell = (v) => v === null ? "<i class='muted'>NULL</i>" : esc(String(v));
  out.innerHTML = `<div class="tbl-wrap"><table class="tbl db-grid"><thead><tr>${cols.map((c) => `<th>${esc(c)}</th>`).join("")}</tr></thead><tbody>${rows.slice(0, 500).map((r) => `<tr>${cols.map((c) => `<td>${cell(r[c])}</td>`).join("")}</tr>`).join("")}</tbody></table></div>${rows.length > 500 ? `<div class="muted">showing first 500 of ${rows.length} rows</div>` : `<div class="muted">${rows.length} row(s)</div>`}`;
}

/* ---------- backups ---------- */
async function renderBackups(id) {
  $("#tab-body").innerHTML = `<div class="card"><h3 class="bk-head">${ic("archive", 15)} ${t("backups")} <span id="bk-mirror" class="pill offline plain" title="${esc(t("mirror_status_disabled_title"))}">${t("mirror_status_disabled")}</span> <span class="badge">zip + sha256</span></h3>
    <div class="row bk-tools">
      <button class="btn primary" data-act="bkCreate" data-sid="${id}">${ic("plus", 14)}<span>${t("create")}</span></button>
      <span class="muted text-sm">keep newest</span>
      <input id="bk-keep" type="number" min="0" value="10" aria-label="Backups to keep">
      <button class="btn sm" data-act="bkCleanup" data-sid="${id}" title="Rotate backups, keeping only the newest N">${ic("sliders", 13)}<span>Rotate</span></button>
      ${state.user?.root_admin ? `<button class="btn sm" data-act="bkMirrorSync" data-sid="${id}">${ic("refresh_ccw", 13)}<span>${t("mirror_sync")}</span></button>` : ""}
      <span id="bk-mirror-err" class="bk-mirror-err" role="status" hidden></span>
    </div>
    <div id="bk-list"><div class="empty">${ic("archive", 40)}<p>${t("loading")}</p></div></div></div>`;
  await bkLoad(id);
}
async function bkLoad(id) {
  const box = $("#bk-list");
  try {
    const res = await api(`/servers/${id}/backups`);
    const bks = res.data || [];
    const mState = mirrorState(res.mirror?.status);
    const mEl = $("#bk-mirror");
    if (mEl) { mEl.className = `pill ${MIRROR_CLS[mState]} plain`; mEl.title = t(`mirror_status_${mState}_title`); mEl.textContent = t(`mirror_status_${mState}`); }
    box.innerHTML = bks.length ? `<div class="file-list">${bks.map((b) => `<div class="file-row"><span class="f-icon">${ic("archive", 16)}</span>
      <div class="bk-main"><b>${esc(b.name)}</b>${b.is_locked ? `<span class="pill running plain">locked</span>` : ""}<div class="f-meta">${fmtDate(b.created_at)}${b.ignored_files ? ` · ignores: ${esc(b.ignored_files.split("\n").filter(Boolean).join(", "))}` : ""}</div></div>
      <span class="f-meta">${fmtBytes(b.size_bytes)}</span>
      <span class="f-meta sha" title="${esc(b.checksum)}"><code>${esc((b.checksum || "").slice(0, 12))}…</code></span>
      <span class="f-actions">
      <a class="icon-btn sm" title="${t("download")}" aria-label="${t("download")}" href="/api/backups/${b.id}/download">${ic("download", 15)}</a>
      <button class="icon-btn sm" title="restore" aria-label="Restore" data-act="bkRestore" data-sid="${id}" data-bid="${b.id}">${ic("refresh_ccw", 15)}</button>
      <button class="icon-btn sm" title="Verify checksum" aria-label="Verify checksum" data-act="bkVerify" data-bid="${b.id}">${ic("check", 14)}</button>
      <button class="icon-btn sm" title="${b.is_locked ? "Unlock" : "Lock"}" aria-label="${b.is_locked ? "Unlock" : "Lock"}" data-act="bkLock" data-bid="${b.id}" data-on="${b.is_locked ? "0" : "1"}">${ic("lock", 14)}</button>
      <button class="icon-btn sm danger" title="${t("delete")}" aria-label="${t("delete")}" data-act="bkDel" data-bid="${b.id}">${ic("trash", 15)}</button>
    </span></div>`).join("")}</div>` : emptyState("archive", t("es_bk_t"), t("es_bk_h"), { act: "bkCreate", data: ` data-sid="${id}"`, actLabel: t("create") });
  } catch (e) {
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_backups")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="bkRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
async function bkCreate(id) {
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("archive", 16)} ${t("bk_create_title")}</b><button class="icon-btn" data-act="closeModal" aria-label="${t("close")}">${ic("x", 16)}</button></div>
    <div class="modal-pad">
      <div class="field"><label>${t("bk_name_opt")} <small>${t("bk_optional")}</small></label><input id="bk-name" placeholder="backup-20260808-120000" spellcheck="false"></div>
      <div class="field"><label>${t("bk_ignore")} <small>${t("bk_ignore_hint")}</small></label><textarea id="bk-ignore" rows="4" placeholder="*.log&#10;cache/&#10;tmp/**" spellcheck="false"></textarea></div>
      <p class="muted text-sm m-0">${t("bk_ignore_note")}</p>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="bkDoCreate" data-sid="${id}">${ic("plus", 14)}<span>${t("create")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
  $("#bk-name").focus();
}
async function bkDoCreate(id, btn) {
  const payload = { name: $("#bk-name").value.trim() || null, ignore: $("#bk-ignore").value };
  try {
    await api(`/servers/${id}/backups`, { method: "POST", body: JSON.stringify(payload) });
    const m = btn ? btn.closest(".modal") : null;
    (m || $(".modal"))?.remove();
    toast(t("created"), "success");
    await bkLoad(id);
  } catch (e) { toast(e.message, "error"); }
}
async function bkLock(bid, locked) {
  try {
    await api(`/backups/${bid}/lock`, { method: "POST", body: JSON.stringify({ locked }) });
    toast(locked ? "Locked" : "Unlocked", "success");
    const sid = state.server?.id || state.fileId;
    if (sid) await bkLoad(sid);
  } catch (e) { toast(e.message, "error"); }
}
async function bkVerify(bid) {
  try {
    const res = await api(`/backups/${bid}/verify`);
    const ok = !!(res.ok && res.checksum_ok);
    const btn = document.querySelector(`[data-act="bkVerify"][data-bid="${bid}"]`);
    const row = btn?.closest(".file-row");
    let cell = row?.querySelector(".bk-verify");
    if (!cell && row) { cell = document.createElement("span"); cell.className = "bk-verify"; row.appendChild(cell); }
    if (cell) { cell.className = "bk-verify " + (ok ? "ok" : "bad"); cell.textContent = ok ? "checksum ok" : "checksum MISMATCH"; }
    toast(ok ? "Backup verified" : "Backup checksum mismatch!", ok ? "success" : "error");
  } catch (e) { toast(e.message, "error"); }
}
async function bkCleanup(id) {
  const keep = Math.max(0, parseInt($("#bk-keep")?.value || "0", 10));
  try {
    const res = await api(`/servers/${id}/backups/cleanup`, { method: "POST", body: JSON.stringify({ keep }) });
    toast(`Rotation removed ${res.removed ?? 0} old backup(s)`, "success");
    await bkLoad(id);
  } catch (e) { toast(e.message, "error"); }
}
async function bkRestore(id, bid, btn) {
  if (!await vpConfirm(t("confirm_restore"))) return;
  if (btn) { btn.disabled = true; btn.innerHTML = `${ic("refresh_ccw", 14)}<span>Restoring…</span>`; }
  try {
    await api(`/backups/${bid}/restore`, { method: "POST", timeout: 120000 });
    toast(t("t_restored"), "success");
    if (id) await bkLoad(id);
  } catch (e) { toast(e.message, "error"); }
  if (btn) { btn.disabled = false; btn.innerHTML = `${ic("refresh_ccw", 14)}<span>Restore</span>`; }
}
async function bkDel(bid) {
  if (!await vpConfirm(t("confirm_delete"))) return;
  try {
    await api(`/backups/${bid}/delete`, { method: "DELETE" });
    toast(t("deleted"), "success");
    const sid = state.server?.id || state.fileId;
    if (sid) await bkLoad(sid);
  } catch (e) { toast(e.message, "error"); }
}
async function bkMirrorSync(id, btn) {
  if (!await vpConfirm(t("mirror_sync_confirm"), t("mirror_sync"))) return;
  const errBox = $("#bk-mirror-err");
  if (errBox) { errBox.hidden = true; errBox.textContent = ""; }
  if (btn) { btn.disabled = true; btn.innerHTML = `${ic("refresh_ccw", 13)}<span>${t("mirror_sync_running")}</span>`; }
  try {
    const res = await api("/backups/mirror/sync", { method: "POST", timeout: 120000 });
    const st = mirrorState(res.mirror_status);
    toast(t("mirror_sync_done").replace("{copied}", String(res.copied ?? 0)).replace("{removed}", String(res.removed ?? 0)).replace("{status}", t(`mirror_status_${st}`)), "success");
    if (id) await bkLoad(id);
  } catch (e) {
    toast(e.message, "error");
    if (errBox) { errBox.textContent = e.message; errBox.hidden = false; }
  }
  if (btn) { btn.disabled = false; btn.innerHTML = `${ic("refresh_ccw", 13)}<span>${t("mirror_sync")}</span>`; }
}

/* ---------- files: pull from URL ---------- */
function filePull(id) {
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("link", 16)} <span class="modal-title">Pull from URL</span></b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div>
    <div class="modal-pad">
      <div class="field"><label>URL *</label><div class="field-input">${ic("link", 14)}<input id="pull-url" placeholder="https://example.com/file.zip" spellcheck="false"></div></div>
      <div class="field"><label>Filename <small>optional - defaults to the URL basename</small></label><div class="field-input">${ic("file", 14)}<input id="pull-name" placeholder="auto" spellcheck="false"></div></div>
      <p class="muted text-sm mt-2">Downloads into <code>${esc(state.filePath)}</code>. Private and loopback sources are rejected.</p>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="pullStart" data-sid="${id}">${ic("download", 14)}<span>${t("save")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
  $("#pull-url").focus();
}
async function pullStart(id) {
  const url = $("#pull-url").value.trim();
  if (!url) { toast("URL is required", "error"); return; }
  const filename = $("#pull-name").value.trim() || null;
  let tid;
  try {
    const res = await api(`/servers/${id}/files/pull`, { method: "POST", body: JSON.stringify({ url, path: state.filePath, filename }) });
    tid = res.data ? res.data.id : res.id;
  } catch (e) { toast(e.message, "error"); return; }
  const modal = $(".modal"); // the pull dialog we just submitted from
  state.pullModal = modal || null;
  const card = $(".modal-card");
  card.innerHTML = `<div class="modal-head"><b>${ic("download", 16)} ${t("pulling_file")}</b><button class="icon-btn" data-act="pullCancel" data-sid="${id}" data-tid="${tid}" aria-label="${t("cancel")}">${ic("x", 16)}</button></div>
    <div class="modal-pad">
      <div class="metric-line"><span class="muted ellipsis">${esc(url)}</span></div>
      <div class="metric-line"><span id="pull-status">starting...</span><b id="pull-bytes"></b></div>
      <div class="progress m-10-0"><div id="pull-bar"></div></div>
    </div>
    <div class="modal-foot"><button class="btn danger" data-act="pullCancel" data-sid="${id}" data-tid="${tid}">${ic("x", 14)}<span>${t("cancel")}</span></button></div>`;
  pullPoll(id, tid);
}
async function pullPoll(id, tid) {
  let res;
  try { res = await api(`/servers/${id}/files/pull/${tid}`); }
  catch (e) { toast(e.message, "error"); return; }
  const s = res.data || res;
  const pct = s.total ? Math.min(100, Math.round((s.received / s.total) * 100)) : (s.status === "running" ? 0 : 100);
  const bar = $("#pull-bar");
  if (bar) bar.style.width = pct + "%";
  const bytes = $("#pull-bytes");
  if (bytes) bytes.textContent = s.total ? `${fmtBytes(s.received)} / ${fmtBytes(s.total)}` : fmtBytes(s.received);
  const st = $("#pull-status");
  if (!st) return;
  if (s.status === "running") {
    st.textContent = s.phase === "pushing" ? "uploading to node..." : `downloading... ${pct}%`;
    setTimeout(() => pullPoll(id, tid), 500);
    return;
  }
  if (s.status === "done") {
    st.textContent = "done";
    toast(t("downloaded"), "success");
    setTimeout(() => state.pullModal?.remove(), 400);
    await loadFiles(id, state.filePath);
    return;
  }
  if (s.status === "cancelled") {
    st.textContent = "cancelled";
    toast(t("t_transfer_cancelled"), "success");
    setTimeout(() => state.pullModal?.remove(), 400);
    return;
  }
  st.textContent = "failed: " + (s.error || "unknown error");
  toast(s.error || "Pull failed", "error");
}
async function pullCancel(id, tid) {
  try { await api(`/servers/${id}/files/pull/${tid}`, { method: "DELETE" }); toast(t("t_cancelling")); pullPoll(id, tid); }
  catch (e) { toast(e.message, "error"); }
}

/* ---------- schedules ---------- */
const SCH_ACTIONS = ["start", "stop", "restart", "kill", "command", "backup", "notify"];
let schDraft = [{ action: "restart", payload: "", sequence: 1 }]; // task-builder draft

/* ---------- Flow Gate conditions ---------- */
/* Canonical condition JSON, exactly as the backend scheduler validates and
   stores it (src/services/scheduler.rs):
     exit    {"kind":"exit","task_index":N,"code":C}              (N < own index)
     signal  {"kind":"signal","event":E,"server_id":S,"timeout_s":T}  (1<=T<=3600)
     none    absent / null / {"kind":"none"}
   The API returns conditions as parsed JSON values with alphabetically sorted
   keys (serde_json BTreeMap), so re-emitting an untouched parsed object stays
   byte-identical; fresh edits are built in the same sorted order. Unrecognized
   kinds (e.g. from a newer backend) are preserved verbatim and surfaced as a
   warning instead of being silently dropped. */
function schCondFromValue(v) {
  let obj = v;
  if (typeof obj === "string") { try { obj = JSON.parse(obj); } catch { return { kind: "unknown", raw: v }; } }
  if (obj === null || obj === undefined) return { kind: "none", raw: null };
  if (typeof obj !== "object" || Array.isArray(obj)) return { kind: "unknown", raw: JSON.stringify(obj) };
  if (obj.kind === "none") return { kind: "none", raw: null };
  if (obj.kind === "exit") return { kind: "exit", taskIndex: obj.task_index ?? 0, code: obj.code ?? 0, raw: null };
  if (obj.kind === "signal") return { kind: "signal", event: obj.event ?? "", serverId: obj.server_id, timeout: obj.timeout_s ?? 60, raw: null };
  return { kind: "unknown", raw: JSON.stringify(obj) };
}
/* Serialize a draft task's gate into the API `condition` value. undefined for
   "no gate" so the key is omitted from the JSON body and the backend stores
   NULL. condRaw (a preserved legacy/unknown gate) is re-emitted verbatim. */
function schCondBody(tk) {
  if (tk.condRaw != null) return JSON.parse(tk.condRaw);
  if (tk.gate === "exit") return { code: tk.gateCode, kind: "exit", task_index: tk.gateTask };
  if (tk.gate === "signal") return { event: tk.gateEvent, kind: "signal", server_id: tk.gateServer, timeout_s: tk.gateTimeout };
  return undefined;
}
/* Build the API `tasks` array shared by create (POST) and edit (PATCH): one
   entry per draft task in order, with the canonical `condition` attached when
   the task has a gate. Edit sends the FULL ordered array in a single atomic
   PATCH (the backend swaps the chain in one BEGIN IMMEDIATE transaction and
   rejects invalid batches before any write), so nothing is ever deleted
   client-side first. */
function schBuildTasks(draft) {
  return draft.map((tk, i) => {
    const body = { action: tk.action, payload: tk.payload, sequence: i + 1 };
    const cond = schCondBody(tk);
    if (cond !== undefined) body.condition = cond;
    return body;
  });
}
/* Client-side mirror of the backend bounds (validate_condition): exit gates
   must reference an earlier task with an integer code; signal gates need a
   non-empty event and an integer timeout in 1..=3600. Returns an i18n message
   (via tr) or null when the gate is valid. */
function schCondError(tk, ownIndex, tr) {
  if (tk.condRaw != null) return null;
  if (tk.gate === "exit") {
    if (!Number.isInteger(tk.gateTask) || tk.gateTask < 0 || tk.gateTask >= ownIndex) return tr("gate_err_index");
    if (!Number.isInteger(tk.gateCode)) return tr("gate_err_code");
    return null;
  }
  if (tk.gate === "signal") {
    if (typeof tk.gateEvent !== "string" || !tk.gateEvent.trim()) return tr("gate_err_event");
    if (!Number.isInteger(tk.gateTimeout) || tk.gateTimeout < 1 || tk.gateTimeout > 3600) return tr("gate_err_timeout");
    return null;
  }
  return null;
}
/* Human-readable gate chip text (1-based task numbers), translated via tr. */
function gateChipText(cond, tr) {
  const c = schCondFromValue(cond);
  if (c.kind === "exit") return tr("gate_after_task").replace("{n}", String(c.taskIndex + 1)).replace("{code}", String(c.code));
  if (c.kind === "signal") return tr("gate_wait_signal").replace("{event}", c.event).replace("{s}", String(c.timeout));
  if (c.kind === "unknown") return tr("gate_unknown").replace("{raw}", c.raw);
  return null;
}
function gateChipHtml(cond, tr) {
  const text = gateChipText(cond, tr);
  return text ? `<span class="gate-chip">${esc(text)}</span>` : "";
}
const mkSchTask = (sid) => ({ action: "restart", payload: "", sequence: 1, gate: "none", gateTask: 0, gateCode: 0, gateEvent: "site.updated", gateTimeout: 60, gateServer: +sid, condRaw: null });

async function renderSchedules(id) {
  $("#tab-body").innerHTML = `<div class="card"><h3>${ic("clock", 15)} ${t("schedules")} <span class="badge">cron</span></h3>
    <div class="row mb-12"><button class="btn primary" data-act="schCreate" data-sid="${id}">${ic("plus", 14)}<span>${t("create")}</span></button><span class="muted text-sm">start / stop / restart / kill / command / backup / notify</span></div>
    <div id="sch-list"><div class="empty">${ic("clock", 40)}<p>${t("loading")}</p></div></div>
    <small class="muted">Cron format: <code>sec min hour day month weekday</code> — daily 04:00: <code>0 0 4 * * *</code></small>
  </div>`;
  await schLoad(id);
}
async function schLoad(id) {
  const box = $("#sch-list");
  try {
    const res = await api(`/servers/${id}/schedules`);
    const schs = res.data || [];
    box.innerHTML = schs.length ? `<div class="file-list">${schs.map((s) => `<div class="file-row">
      <span class="f-icon">${ic("clock", 16)}</span>
      <div class="sch-main"><b>${esc(s.name)}</b><div class="f-meta">${(s.tasks || []).map((tk) => `${esc(tk.action)}${tk.payload ? `: ${esc(tk.payload)}` : ""}`).join(" → ") || "no tasks"}</div>${(s.tasks || []).some((tk) => gateChipText(tk.condition, t)) ? `<div class="gate-line">${(s.tasks || []).map((tk) => gateChipHtml(tk.condition, t)).join("")}</div>` : ""}</div>
      <code>${esc(s.cron_expr)}</code>
      <span class="pill ${s.enabled ? "running" : "offline"}"><i></i>${s.enabled ? "on" : "off"}</span>
      <span class="f-meta">next: ${s.next_run_at ? fmtDate(s.next_run_at) : "—"}</span>
      <span class="f-actions">
      <button class="icon-btn sm" title="${s.enabled ? "Pause" : "Enable"}" aria-label="${s.enabled ? "Pause" : "Enable"}" data-act="schToggle" data-sid="${s.id}" data-on="${s.enabled ? "0" : "1"}">${s.enabled ? ic("pause", 14) : ic("play", 14)}</button>
      <button class="icon-btn sm" title="${t("a_run_now")}" aria-label="${t("a_run_now")}" data-act="schRun" data-sid="${s.id}">${ic("zap", 14)}</button>
      <button class="icon-btn sm" title="${t("edit")}" aria-label="${t("edit")}" data-act="schEdit" data-sid="${s.id}" data-schid="${s.id}" data-server="${id}">${ic("pencil", 14)}</button>
      <button class="icon-btn sm" title="${t("a_run_history")}" aria-label="${t("a_run_history")}" data-act="schRuns" data-sid="${s.id}" data-server="${id}">${ic("activity", 14)}</button>
      <button class="icon-btn sm danger" title="${t("delete")}" aria-label="${t("delete")}" data-act="schDel" data-sid="${s.id}">${ic("trash", 14)}</button>
    </span></div>`).join("")}</div>` : emptyState("clock", t("es_sch_t"), t("es_sch_h"), { act: "schCreate", data: ` data-sid="${id}"`, actLabel: t("create") });
  } catch (e) {
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_schedules")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="schRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
function renderSchTasks() {
  const box = $("#sch-tasks");
  if (!box) return;
  box.innerHTML = schDraft.length ? schDraft.map((tk, i) => {
    const gateOpt = (v, label) => `<option value="${v}"${tk.gate === v ? " selected" : ""}>${label}</option>`;
    const prev = i === 0
      ? `<option value="0" disabled>${esc(t("gate_no_earlier"))}</option>`
      : Array.from({ length: i }, (_, k) => `<option value="${k}"${+tk.gateTask === k ? " selected" : ""}>${esc(t("gate_task_opt").replace("{n}", String(k + 1)))}</option>`).join("");
    const exitFields = tk.gate === "exit" ? `
      <select data-sch-task="gateTask" data-idx="${i}" data-act-change="schTaskGateField" aria-label="${esc(t("gate_previous_task"))}" title="${esc(t("gate_previous_task"))}">${prev}</select>
      <input data-sch-task="gateCode" data-idx="${i}" type="number" step="1" value="${Number.isNaN(tk.gateCode) ? "" : esc(tk.gateCode)}" placeholder="0" aria-label="${esc(t("gate_expected_code"))}" title="${esc(t("gate_expected_code"))}">` : "";
    const customEvt = !WH_EVENTS.includes(tk.gateEvent);
    const signalFields = tk.gate === "signal" ? `
      <select data-sch-task="gateEvent" data-idx="${i}" data-act-change="schTaskGateField" aria-label="${esc(t("gate_event"))}" title="${esc(t("gate_event"))}">${WH_EVENTS.map((ev) => `<option value="${esc(ev)}"${!customEvt && tk.gateEvent === ev ? " selected" : ""}>${esc(ev)}</option>`).join("")}<option value="__custom"${customEvt ? " selected" : ""}>${esc(t("gate_custom"))}</option></select>
      ${customEvt ? `<input data-sch-task="gateEventCustom" data-idx="${i}" value="${esc(tk.gateEvent)}" placeholder="${esc(t("gate_custom_ph"))}" aria-label="${esc(t("gate_event"))}">` : ""}
      <input data-sch-task="gateTimeout" data-idx="${i}" type="number" min="1" max="3600" step="1" value="${Number.isNaN(tk.gateTimeout) ? "" : esc(tk.gateTimeout)}" aria-label="${esc(t("gate_timeout"))}" title="${esc(t("gate_timeout"))}">
      <span class="sch-gate-lock" title="${esc(t("gate_lock_title"))}">${ic("server", 12)}${esc(t("gate_lock"))}</span>` : "";
    return `<div class="sch-task">
      <div class="sch-task-line">
        <select data-sch-task="action" data-idx="${i}" data-act-change="schTaskAction" aria-label="${esc(t("gate_task_opt").replace("{n}", String(i + 1)))} ${esc(t("gate"))}">${SCH_ACTIONS.map((a) => `<option value="${a}"${a === tk.action ? " selected" : ""}>${a}</option>`).join("")}</select>
        <input data-sch-task="payload" data-idx="${i}" value="${esc(tk.payload)}" placeholder="${tk.action === "command" ? t("sch_cmd_ph") : tk.action === "backup" ? t("sch_backup_ph") : t("sch_payload_ph")}" aria-label="${t("sch_payload_label")}">
        <button class="icon-btn sm danger" title="${t("delete")}" aria-label="${t("delete")}" data-act="schTaskDel" data-idx="${i}">${ic("trash", 13)}</button>
      </div>
      <div class="sch-gate">
        <span class="sch-gate-label">${esc(t("gate"))}</span>
        <select data-sch-task="gate" data-idx="${i}" data-act-change="schTaskGate" aria-label="${esc(t("gate"))}">${gateOpt("none", t("gate_none"))}${gateOpt("exit", t("gate_exit"))}${gateOpt("signal", t("gate_signal"))}</select>
        ${exitFields}${signalFields}
      </div>
    </div>`;
  }).join("") : `<div class="muted">No tasks — add at least one.</div>`;
}
function schTaskAdd() {
  const sidEl = $('[data-act="schSave"]');
  schDraft.push(mkSchTask(+((sidEl && sidEl.dataset.sid) || state.server?.id || 0)));
  schDraft[schDraft.length - 1].sequence = schDraft.length;
  renderSchTasks();
}
function schTaskDel(idx) { schDraft.splice(idx, 1); schDraft.forEach((tk, i) => tk.sequence = i + 1); renderSchTasks(); }
function schTaskAction(el) {
  const i = +el.dataset.idx;
  if (!schDraft[i]) return;
  schDraft[i].action = el.value;
  const p = document.querySelector(`[data-sch-task="payload"][data-idx="${i}"]`);
  if (p) p.placeholder = schDraft[i].action === "command" ? "command to send" : schDraft[i].action === "backup" ? "backup name (optional)" : "payload (optional)";
}
function schTaskGate(el) {
  const i = +el.dataset.idx;
  if (!schDraft[i]) return;
  schDraft[i].gate = el.value;
  schDraft[i].condRaw = null; // explicit choice replaces a preserved legacy gate
  renderSchTasks();
}
function schTaskGateField(el) {
  const i = +el.dataset.idx;
  if (!schDraft[i]) return;
  if (el.dataset.schTask === "gateTask") schDraft[i].gateTask = +el.value;
  else if (el.dataset.schTask === "gateEvent" && el.value === "__custom") { schDraft[i].gateEvent = ""; renderSchTasks(); }
  else if (el.dataset.schTask === "gateEvent") schDraft[i].gateEvent = el.value;
}
function schCreate(id) {
  schDraft = [mkSchTask(id)];
  openSchModal(id, null);
}
function schEdit(id, schId, serverId) {
  // fetch fresh schedule JSON (list rows carry tasks, but re-fetch is authoritative)
  api(`/servers/${serverId}/schedules`).then((res) => {
    const found = (res.data || []).find((x) => x.id === +schId);
    if (!found) { toast(t("e_sch_not_found"), "error"); return; }
    const unknown = [];
    schDraft = (found.tasks || []).map((tk, i) => {
      const d = mkSchTask(serverId);
      d.sequence = i + 1;
      d.action = tk.action;
      d.payload = tk.payload || "";
      const c = schCondFromValue(tk.condition);
      if (c.kind === "exit") { d.gate = "exit"; d.gateTask = c.taskIndex; d.gateCode = c.code; }
      else if (c.kind === "signal") { d.gate = "signal"; d.gateEvent = c.event; d.gateTimeout = c.timeout; d.gateServer = c.serverId; }
      else if (c.kind === "unknown") { d.condRaw = c.raw; unknown.push(c.raw); }
      return d;
    });
    if (!schDraft.length) schDraft = [mkSchTask(serverId)];
    if (unknown.length) toast(t("gate_warn_unknown").replace("{raw}", unknown.join(", ")), "warn");
    openSchModal(serverId, found);
  }).catch((e) => toast(e.message, "error"));
}
function openSchModal(serverId, sch) {
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("clock", 16)} ${sch ? t("edit_schedule") : t("new_schedule")}</b><button class="icon-btn" data-act="closeModal" aria-label="${t("close")}">${ic("x", 16)}</button></div>
    <div class="modal-pad">
      <div class="field"><label>${t("name")}</label><input id="sch-name" value="${sch ? esc(sch.name) : ""}" placeholder="${t("sch_name_ph")}"></div>
      <div class="field"><label>${t("sch_cron_label")} <code>sec min hour day month weekday</code></label><input id="sch-cron" value="${sch ? esc(sch.cron_expr) : "0 0 4 * * *"}" placeholder="0 0 4 * * *" spellcheck="false"></div>
      <div class="field-row mb-16">
        <label class="check-row"><input type="checkbox" id="sch-enabled" ${!sch || sch.enabled ? "checked" : ""}><span class="check-box">${ic("check", 12)}</span>${t("enabled")}</label>
        <label class="muted text-sm">${t("retries")} <input id="sch-retries" type="number" min="0" value="${sch ? (sch.max_retries ?? 0) : 0}"></label>
        <label class="muted text-sm">${t("sch_backoff")} <input id="sch-backoff" type="number" min="0" value="${sch ? (sch.retry_backoff_s ?? 30) : 30}"></label>
      </div>
      <div class="field"><label>${t("sch_tasks")}</label><div id="sch-tasks"></div>
        <button class="btn sm ghost" data-act="schTaskAdd">${ic("plus", 12)}<span>${t("add_task")}</span></button>
      </div>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="schSave" data-sid="${serverId}" data-schid="${sch ? sch.id : 0}">${ic("check", 14)}<span>${sch ? t("save_changes") : t("create")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
  renderSchTasks();
  $("#sch-tasks").addEventListener("input", (e) => {
    const el = e.target;
    const i = +el.dataset.idx;
    if (!schDraft[i]) return;
    const f = el.dataset.schTask;
    if (f === "payload") schDraft[i].payload = el.value;
    else if (f === "gateCode") schDraft[i].gateCode = el.value === "" ? NaN : +el.value;
    else if (f === "gateTimeout") schDraft[i].gateTimeout = el.value === "" ? NaN : +el.value;
    else if (f === "gateEventCustom") schDraft[i].gateEvent = el.value;
  });
  $("#sch-name").focus();
}
async function schSave(serverId, schId, btn) {
  const name = $("#sch-name").value.trim();
  const cron = $("#sch-cron").value.trim();
  if (!name || !cron) { toast(t("e_name_cron"), "error"); return; }
  if (!schDraft.length) { toast(t("e_one_task"), "error"); return; }
  // client-side mirror of the backend gate bounds — fail before any write
  for (let i = 0; i < schDraft.length; i++) {
    const err = schCondError(schDraft[i], i, t);
    if (err) { toast(`${t("gate")} ${i + 1}: ${err}`, "error"); return; }
  }
  const max_retries = Math.max(0, parseInt($("#sch-retries").value || "0", 10));
  const retry_backoff_s = Math.max(0, parseInt($("#sch-backoff").value || "30", 10));
  const enabled = $("#sch-enabled").checked;
  try {
    if (schId) {
      // One atomic PATCH: the backend validates the whole task batch up front
      // and swaps the chain in a single BEGIN IMMEDIATE transaction. A rejected
      // gate (e.g. unknown kind) fails before any write and leaves the existing
      // tasks untouched — nothing is deleted client-side first, so the draft
      // survives and the backend error is shown as-is.
      await api(`/schedules/${schId}`, { method: "PATCH", body: JSON.stringify({ name, cron_expr: cron, enabled, max_retries, retry_backoff_s, tasks: schBuildTasks(schDraft) }) });
    } else {
      await api(`/servers/${serverId}/schedules`, { method: "POST", body: JSON.stringify({ name, cron_expr: cron, enabled, max_retries, retry_backoff_s, tasks: schBuildTasks(schDraft) }) });
    }
    const m = btn ? btn.closest(".modal") : null;
    (m || $(".modal"))?.remove();
    toast(schId ? t("saved") : t("created"), "success");
    await schLoad(serverId);
  } catch (e) { toast(e.message, "error"); }
}
async function schRuns(serverId, schId) {
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.innerHTML = `<div class="modal-card big"><div class="modal-head"><b>${ic("clock", 15)} Run history</b><button class="icon-btn" data-act="closeModal" aria-label="Close">${ic("x", 16)}</button></div><div id="sch-runs"><div class="empty">${ic("clock", 40)}<p>${t("loading")}</p></div></div></div>`;
  document.body.appendChild(modal);
  try {
    const [rres, sres] = await Promise.all([api(`/servers/${serverId}/schedules/${schId}/runs`), api(`/servers/${serverId}/schedules`)]);
    const sch = (sres.data || []).find((x) => x.id === +schId);
    const gates = (sch?.tasks || []).map((tk) => gateChipHtml(tk.condition, t)).filter(Boolean);
    const rows = rres.data || [];
    const box = $("#sch-runs");
    const pill = (st) => st === "success" ? "running" : st === "failed" ? "error" : "warn";
    box.innerHTML = (gates.length ? `<div class="gate-line mb-8">${gates.join("")}</div>` : "") + (rows.length ? `<div class="file-list">${rows.map((r) => `<div class="file-row">
      <span class="f-icon">${ic("zap", 14)}</span>
      <div class="sch-main"><b>${fmtDate(r.triggered_at)}</b><div class="f-meta">attempt ${r.attempt}${r.finished_at ? ` · finished ${fmtDate(r.finished_at)}` : ""}</div></div>
      <span class="pill ${pill(r.status)}"><i></i>${esc(r.status)}</span>
      ${r.log ? `<div class="sch-run-log" title="${esc(r.log)}">${esc(r.log.slice(0, 160))}</div>` : ""}
    </div>`).join("")}</div>` : emptyState("clock", t("es_runs_t"), t("es_runs_h")));
  } catch (e) { $("#sch-runs").innerHTML = `<div class="empty">${ic("alert", 40)}<p>${esc(e.message)}</p></div>`; }
}
async function schToggle(id, on) { try { await api(`/schedules/${id}/toggle/${on}`, { method: "POST" }); await schLoad(state.server?.id); } catch (e) { toast(e.message, "error"); } }
async function schRun(id) { try { await api(`/schedules/${id}/run`, { method: "POST" }); toast(t("t_triggered"), "success"); } catch (e) { toast(e.message, "error"); } }
async function schDel(id) { if (!await vpConfirm(t("confirm_delete"))) return; try { await api(`/schedules/${id}`, { method: "DELETE" }); await schLoad(state.server?.id); } catch (e) { toast(e.message, "error"); } }
/* ---------- watchers ---------- */
// Per-server console watchers: log-pattern rules that fire an action on match.
// Original angle vs a plain rule list: each row surfaces live-fire telemetry
// (trigger count + relative last-fired) so operators see which rules are hot.
let watcherDraft = null;
async function renderWatchers(id) {
  $("#tab-body").innerHTML = `<div class="card"><h3>${ic("bell", 15)} ${t("watchers")} <span class="badge">${t("watchers_badge")}</span></h3>
    <div class="row mb-12"><button class="btn primary" data-act="watcherCreate" data-sid="${id}">${ic("plus", 14)}<span>${t("create")}</span></button><span class="muted text-sm">${t("watchers_hint")}</span></div>
    <div id="watcher-list"><div class="empty">${ic("bell", 40)}<p>${t("loading")}</p></div></div>
  </div>`;
  await watcherLoad(id);
}
async function watcherLoad(id) {
  const box = $("#watcher-list");
  if (!box) return;
  try {
    const res = await api(`/servers/${id}/console/watchers`);
    const ws = res.data || [];
    if (!ws.length) { box.innerHTML = `<div class="empty">${ic("bell", 40)}<p>${t("watchers_empty")}</p></div>`; return; }
    box.innerHTML = `<div class="file-list">${ws.map((w) => watcherRow(id, w)).join("")}</div>`;
  } catch (e) { box.innerHTML = `<div class="empty">${ic("bell", 40)}<p>${esc(e.message)}</p></div>`; }
}
function watcherRow(id, w) {
  const enc = encodeURIComponent(JSON.stringify(w));
  const act = watcherActionLabel(w);
  const fired = w.last_fired_at
    ? `<span class="wt-fired">${ic("activity", 12)} ${esc(t("watcher_fired").replace("{n}", w.trigger_count).replace("{ago}", fmtDate(w.last_fired_at)))}</span>`
    : `<span class="muted text-sm">${t("watcher_never")}</span>`;
  return `<div class="file-row watcher-row${w.enabled ? "" : " off"}">
    <span class="f-icon">${ic(w.is_regex ? "code" : "search", 16)}</span>
    <div class="wt-main"><b>${esc(w.name)}</b>
      <div class="f-meta"><code>${esc(w.pattern)}</code> <span class="sep">→</span> <span class="pill plain">${esc(act)}</span></div>
      <div class="f-meta">${fired}${w.cooldown_secs > 0 ? ` <span class="muted text-sm">${ic("clock", 12)} ${w.cooldown_secs}s</span>` : ""}</div>
    </div>
    <span class="f-actions">
      <button class="icon-btn sm" title="${w.enabled ? t("pause") : t("enable")}" aria-label="${w.enabled ? t("pause") : t("enable")}" data-act="watcherToggle" data-sid="${id}" data-w="${enc}">${w.enabled ? ic("pause", 14) : ic("play", 14)}</button>
      <button class="icon-btn sm" title="${t("edit")}" aria-label="${t("edit")}" data-act="watcherEdit" data-sid="${id}" data-w="${enc}">${ic("edit", 14)}</button>
      <button class="icon-btn sm danger" title="${t("delete")}" aria-label="${t("delete")}" data-act="watcherDel" data-sid="${id}" data-wid="${w.id}" data-name="${esc(w.name)}">${ic("x", 14)}</button>
    </span>
  </div>`;
}
function watcherActionLabel(w) {
  if (w.action === "notify") return `${t("wt_notify")}: ${w.action_payload || "info"}`;
  if (w.action === "command") return `${t("wt_command")}: ${w.action_payload}`;
  return t(w.action === "restart" ? "wt_restart" : "wt_stop");
}
function openWatcherModal(serverId, w) {
  watcherDraft = w ? { ...w } : { action: "notify", is_regex: false, action_payload: "info", cooldown_secs: 0, enabled: true };
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("bell", 15)} ${w ? t("watcher_edit") : t("watcher_new")}</b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div>
    <div class="modal-pad">
      <div class="field"><label>${t("name")}</label><input id="wt-name" autocomplete="off" value="${w ? esc(w.name) : ""}" placeholder="${t("watcher_name_ph")}"></div>
      <div class="field"><label>${t("watcher_pattern")}</label><input id="wt-pattern" autocomplete="off" value="${w ? esc(w.pattern) : ""}" placeholder="${t("watcher_pattern_ph")}"></div>
      <div class="field"><label class="check"><input id="wt-regex" type="checkbox"${w && w.is_regex ? " checked" : ""}> ${t("watcher_regex")}</label></div>
      <div class="field"><label>${t("watcher_action")}</label>
        <select id="wt-action" data-act="watcherActionChange" data-act-change>
          <option value="notify"${watcherDraft.action === "notify" ? " selected" : ""}>${t("wt_notify")}</option>
          <option value="command"${watcherDraft.action === "command" ? " selected" : ""}>${t("wt_command")}</option>
          <option value="restart"${watcherDraft.action === "restart" ? " selected" : ""}>${t("wt_restart")}</option>
          <option value="stop"${watcherDraft.action === "stop" ? " selected" : ""}>${t("wt_stop")}</option>
        </select>
      </div>
      <div class="field" id="wt-payload-field">${watcherPayloadField(watcherDraft)}</div>
      <div class="field"><label>${t("watcher_cooldown")}</label><input id="wt-cooldown" type="number" min="0" value="${w ? (w.cooldown_secs || 0) : 0}"><span class="muted text-sm">${t("watcher_cooldown_hint")}</span></div>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="watcherSave" data-sid="${serverId}" data-wid="${w ? w.id : 0}">${ic("check", 14)}<span>${w ? t("save_changes") : t("create")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
  const inp = modal.querySelector("#wt-name");
  if (inp) inp.focus();
}
function watcherPayloadField(w) {
  if (w.action === "notify") {
    const lv = w.action_payload || "info";
    return `<label>${t("watcher_level")}</label><select id="wt-payload">
      ${["info", "warn", "error"].map((l) => `<option value="${l}"${lv === l ? " selected" : ""}>${t("lvl_" + l)}</option>`).join("")}
    </select>`;
  }
  if (w.action === "command") {
    return `<label>${t("watcher_command")}</label><input id="wt-payload" autocomplete="off" value="${esc(w.action_payload || "")}" placeholder="${t("watcher_command_ph")}">`;
  }
  return `<p class="muted text-sm">${t("watcher_no_payload")}</p>`;
}
function watcherActionChange(el) {
  if (!watcherDraft) return;
  // Preserve the typed payload per-action so switching back restores it.
  const cur = document.querySelector("#wt-payload");
  if (cur) watcherDraft.action_payload = cur.value;
  watcherDraft.action = el.value;
  if (el.value === "notify" && !["info", "warn", "error"].includes(watcherDraft.action_payload)) watcherDraft.action_payload = "info";
  const field = document.querySelector("#wt-payload-field");
  if (field) field.innerHTML = watcherPayloadField(watcherDraft);
}
async function watcherSave(serverId, watcherId, btn) {
  const name = $("#wt-name").value.trim();
  const pattern = $("#wt-pattern").value;
  if (!name || !pattern.trim()) { toast(t("e_watcher_fields"), "error"); return; }
  const is_regex = $("#wt-regex").checked;
  const action = $("#wt-action").value;
  const payEl = $("#wt-payload");
  const action_payload = payEl ? payEl.value : "";
  if (action === "command" && !action_payload.trim()) { toast(t("e_watcher_command"), "error"); return; }
  const cooldown_secs = Math.max(0, parseInt($("#wt-cooldown").value || "0", 10));
  const body = { name, pattern, is_regex, action, action_payload, cooldown_secs };
  if (btn) btn.disabled = true;
  try {
    if (watcherId) {
      body.enabled = watcherDraft ? watcherDraft.enabled !== false : true;
      await api(`/servers/${serverId}/console/watchers/${watcherId}`, { method: "PUT", body: JSON.stringify(body) });
    } else {
      await api(`/servers/${serverId}/console/watchers`, { method: "POST", body: JSON.stringify(body) });
    }
    const m = btn ? btn.closest(".modal") : null;
    (m || $(".modal"))?.remove();
    await watcherLoad(serverId);
  } catch (e) { toast(e.message, "error"); if (btn) btn.disabled = false; }
}
async function watcherToggle(serverId, w) {
  // No dedicated toggle route: re-send the full record with enabled flipped.
  const body = {
    name: w.name, pattern: w.pattern, is_regex: w.is_regex,
    action: w.action, action_payload: w.action_payload,
    cooldown_secs: w.cooldown_secs, enabled: !w.enabled,
  };
  try {
    await api(`/servers/${serverId}/console/watchers/${w.id}`, { method: "PUT", body: JSON.stringify(body) });
    await watcherLoad(serverId);
  } catch (e) { toast(e.message, "error"); }
}
async function watcherDel(serverId, watcherId, name) {
  if (!await vpConfirm(t("watcher_confirm_del").replace("{name}", name || ""))) return;
  try {
    await api(`/servers/${serverId}/console/watchers/${watcherId}`, { method: "DELETE" });
    await watcherLoad(serverId);
  } catch (e) { toast(e.message, "error"); }
}
/* ---------- sites ---------- */
async function renderSites(id) {
  $("#tab-body").innerHTML = `<div class="card"><h3>${ic("globe", 15)} ${t("sites")} <span class="badge">vhost</span></h3>
    <div class="row mb-12"><button class="btn primary" data-act="siteOpen" data-sid="${id}">${ic("plus", 14)}<span>${t("create")}</span></button><span class="muted text-sm">domain → static dir or proxy upstream</span></div>
    <div id="site-list"><div class="empty">${ic("globe", 40)}<p>${t("loading")}</p></div></div>
  </div>`;
  await siteLoad(id);
}
async function siteLoad(id) {
  try {
    const res = await api(`/servers/${id}/sites`);
    const sites = res.data || [];
    $("#site-list").innerHTML = sites.length ? `<div class="file-list">${sites.map((s) => `<div class="file-row"><span class="f-icon">${ic("globe", 16)}</span><div><b>${esc(s.domain)}</b><div class="f-meta">${s.proxy_type === "proxy" ? `proxy → ${esc(s.upstream)}` : `static · ${esc(s.root_dir)}`}${s.port ? ` · :${esc(s.port)}` : ""}</div></div><span class="pill ${s.enabled ? "running" : "offline"}"><i></i>${s.enabled ? "on" : "off"}</span><span class="f-meta">${s.ssl ? "TLS" : "—"}${s.force_https ? " · force https" : ""}</span><span class="f-actions">
      <button class="icon-btn sm" title="${s.enabled ? "Disable" : "Enable"}" aria-label="${s.enabled ? "Disable" : "Enable"}" data-act="siteToggle" data-sid="${id}" data-site="${s.id}">${s.enabled ? ic("pause", 14) : ic("play", 14)}</button>
      <button class="icon-btn sm" title="Edit" aria-label="Edit" data-act="siteOpen" data-sid="${id}" data-site="${s.id}">${ic("pencil", 14)}</button>
      <button class="icon-btn sm danger" title="Delete" aria-label="Delete" data-act="siteDel" data-sid="${id}" data-site="${s.id}" data-domain="${esc(s.domain)}">${ic("trash", 14)}</button>
    </span></div>`).join("")}</div>` : emptyState("globe", t("es_site_t"), t("es_site_h"), { act: "siteOpen", data: ` data-sid="${id}"`, actLabel: t("create") });
  } catch (e) {
    const box = $("#site-list");
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_sites")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="siteRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
async function siteOpen(id, siteId) {
  let site = null;
  if (siteId) {
    try { site = (await api(`/servers/${id}/sites/${siteId}`)).data; } catch (e) { toast(e.message, "error"); return; }
  }
  const proxy = !!site && site.proxy_type === "proxy";
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("globe", 16)} <span class="modal-title">${site ? `Edit ${esc(site.domain)}` : "New site"}</span></b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div>
    <div class="field"><label>${t("domain")} *</label><div class="field-input">${ic("globe", 14)}<input id="site-domain" value="${site ? esc(site.domain) : ""}" placeholder="example.com" spellcheck="false"></div></div>
    <div class="field"><label>Type</label><select id="site-type" data-act="siteType" data-act-change><option value="static"${proxy ? "" : " selected"}>static — serve a root dir</option><option value="proxy"${proxy ? " selected" : ""}>proxy — reverse to an upstream</option></select></div>
    <div class="field"><label id="site-target-label">${proxy ? "Upstream" : "Root dir"} <code>${proxy ? "http(s)://host:port" : "/path"}</code></label><div class="field-input">${ic(proxy ? "link" : "folder", 14)}<input id="site-target" value="${site ? esc(proxy ? site.upstream : site.root_dir) : ""}" placeholder="${proxy ? "http://localhost:3000" : "/"}" spellcheck="false"></div></div>
    <div class="field"><label>Port <small>optional listen port</small></label><div class="field-input">${ic("server", 14)}<input id="site-port" inputmode="numeric" value="${site && site.port ? esc(site.port) : ""}" placeholder="auto"></div></div>
    <div class="field-row site-checks">
      <label class="check-row"><input type="checkbox" id="site-ssl" ${site && site.ssl ? "checked" : ""}><span class="check-box">${ic("check", 12)}</span>${t("ssl")}</label>
      <label class="check-row"><input type="checkbox" id="site-force" ${site && site.force_https ? "checked" : ""}><span class="check-box">${ic("check", 12)}</span>Force HTTPS</label>
      <label class="check-row"><input type="checkbox" id="site-enabled" ${!site || site.enabled ? "checked" : ""}><span class="check-box">${ic("check", 12)}</span>${t("enabled")}</label>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="siteSave" data-sid="${id}" data-site="${site ? site.id : ""}">${ic("check", 14)}<span>${t("save")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
}
function siteType() {
  const proxy = $("#site-type").value === "proxy";
  $("#site-target-label").innerHTML = proxy ? "Upstream <code>http(s)://host:port</code>" : "Root dir <code>/path</code>";
  $("#site-target").placeholder = proxy ? "http://localhost:3000" : "/";
}
async function siteSave(id, siteId, btn) {
  const domain = $("#site-domain").value.trim();
  if (!domain) { toast(t("e_domain_req"), "error"); return; }
  const proxy = $("#site-type").value === "proxy";
  const target = $("#site-target").value.trim();
  const port = $("#site-port").value.trim();
  const payload = {
    domain,
    proxy_type: proxy ? "proxy" : "static",
    root_dir: proxy ? "/" : (target || "/"),
    upstream: proxy ? target : "",
    port: port ? +port : null,
    ssl: $("#site-ssl").checked,
    force_https: $("#site-force").checked,
    enabled: $("#site-enabled").checked,
  };
  try {
    await api(`/servers/${id}/sites${siteId ? `/${siteId}` : ""}`, { method: siteId ? "PATCH" : "POST", body: JSON.stringify(payload) });
    const m = btn ? btn.closest(".modal") : null;
    (m || $(".modal"))?.remove();
    toast(siteId ? t("saved") : t("created"), "success");
    await siteLoad(id);
  } catch (e) { toast(e.message, "error"); }
}
async function siteToggle(id, siteId) { try { await api(`/servers/${id}/sites/${siteId}/toggle`, { method: "POST" }); await siteLoad(id); } catch (e) { toast(e.message, "error"); } }
async function siteDel(id, siteId, domain) { if (!await vpConfirm(`${t("confirm_delete")} ${domain}?`)) return; try { await api(`/servers/${id}/sites/${siteId}`, { method: "DELETE" }); toast(t("deleted"), "success"); await siteLoad(id); } catch (e) { toast(e.message, "error"); } }

/* ---------- allocations ---------- */
async function renderAllocations(id) {
  $("#tab-body").innerHTML = `<div class="card"><h3>${ic("link", 15)} Network <span class="badge">ports</span></h3>
    <div class="row alloc-row">
      <input id="alloc-port" inputmode="numeric" placeholder="port, e.g. 20001">
      <input id="alloc-notes" placeholder="notes (optional)">
      <button class="btn primary" data-act="allocAdd" data-sid="${id}">${ic("plus", 14)}<span>${t("create")}</span></button>
    </div>
    <div id="alloc-list"><div class="empty">${ic("link", 40)}<p>${t("loading")}</p></div></div>
  </div>`;
  await allocLoad(id);
}

async function allocLoad(id) {
  try {
    const res = await api(`/servers/${id}/allocations`);
    const allocs = res.data || [];
    const primary = allocs.find((a) => a.is_primary);
    updateEndpointChip(primary ? primary.port : null);
    $("#alloc-list").innerHTML = allocs.length ? `<div class="file-list">${allocs.map((a) => `
      <div class="file-row">
        <span class="f-icon">${ic("link", 16)}</span>
          <b>${esc(location.hostname)}:${esc(a.port)}</b>
          ${a.is_primary ? `<span class="pill running plain">primary</span>` : `<span class="badge">${esc(a.node)}</span>`}
          <div class="f-meta">${a.notes ? `${esc(a.notes)} · ` : ""}assigned ${fmtDate(a.assigned_at)}</div>
        </div>
        <span class="f-actions">
          ${a.is_primary
            ? `<button class="icon-btn sm" title="Copy endpoint" aria-label="Copy endpoint" data-act="copyText" data-text="${esc(location.hostname + ":" + a.port)}">${ic("copy", 14)}</button>`
            : `<button class="btn xs ghost" title="Make primary" data-act="allocPromote" data-sid="${id}" data-aid="${esc(a.id)}">${ic("zap", 12)}<span>primary</span></button>`}
          <button class="icon-btn sm" title="Edit notes" aria-label="Edit notes" data-act="allocNotes" data-sid="${id}" data-aid="${esc(a.id)}" data-port="${esc(a.port)}">${ic("pencil", 14)}</button>
          ${a.is_primary ? "" : `<button class="icon-btn sm danger" title="Detach" aria-label="Detach" data-act="allocDel" data-sid="${id}" data-aid="${esc(a.id)}" data-port="${esc(a.port)}">${ic("trash", 14)}</button>`}
        </span>
      </div>`).join("")}</div>`
      : emptyState("link", t("es_alloc_t"), t("es_alloc_h"));
  } catch (e) {
    const box = $("#alloc-list");
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_allocations")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="allocRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}

async function allocAdd(id) {
  const port = $("#alloc-port").value.trim();
  if (!port || !/^\d{1,5}$/.test(port)) { toast(t("e_valid_port"), "error"); return; }
  const notes = $("#alloc-notes").value.trim();
  try {
    await api(`/servers/${id}/allocations`, { method: "POST", body: JSON.stringify({ port: +port, notes }) });
    toast(t("created"), "success");
    $("#alloc-notes").value = "";
    $("#alloc-port").value = "";
    await allocLoad(id);
  } catch (e) { toast(e.message, "error"); }
}

async function allocPromote(id, aid) {
  try {
    await api(`/servers/${id}/allocations/${aid}`, { method: "PATCH", body: JSON.stringify({ primary: true }) });
    toast(t("t_primary_updated"), "success");
    await allocLoad(id);
  } catch (e) { toast(e.message, "error"); }
}

async function allocNotes(id, aid, port) {
  const notes = await vpPrompt(`Notes for port ${port}`);
  if (notes === null || notes === false) return; // cancelled
  try {
    await api(`/servers/${id}/allocations/${aid}`, { method: "PATCH", body: JSON.stringify({ notes }) });
    toast(t("saved"), "success");
    await allocLoad(id);
  } catch (e) { toast(e.message, "error"); }
}

async function allocDel(id, aid, port) {
  if (!await vpConfirm(`Detach port ${port}?`, "Detach endpoint")) return;
  try {
    await api(`/servers/${id}/allocations/${aid}`, { method: "DELETE" });
    toast(t("t_detached"), "success");
    await allocLoad(id);
  } catch (e) { toast(e.message, "error"); }
}

/* Keeps the header endpoint chip in step with the primary allocation after a
   promote/detach, so the visible "host:port" is not stale until reload. */
function updateEndpointChip(primaryPort) {
  const chip = document.querySelector(".server-head .allocation-chip");
  if (!chip) return;
  if (state.server) state.server.port = primaryPort;
  if (primaryPort) {
    chip.classList.remove("muted");
    chip.dataset.text = location.hostname + ":" + primaryPort;
    chip.innerHTML = `${ic("link", 12)}${esc(location.hostname)}:${esc(primaryPort)}${ic("copy", 11)}`;
  } else if (chip.dataset.text) {
    chip.dataset.text = "";
    chip.innerHTML = `<span class="allocation-chip muted">No endpoint</span>`;
  }
}

/* ---------- metric sparkline ---------- */
function pushMetric(id, st) {
  if (state.page !== "server") return;
  state.charts[id] = state.charts[id] || { cpu: [], mem: [], ts: [] };
  const c = state.charts[id];
  c.cpu.push(st.cpu); c.mem.push(st.memory_percent); c.ts.push(Date.now());
  if (c.cpu.length > 60) { c.cpu.shift(); c.mem.shift(); c.ts.shift(); }
  const holder = $("#metric-chart");
  if (!holder) return;
  holder.innerHTML = `<div class="card"><h3>${ic("activity", 15)} CPU (60s)</h3><div class="sparkline">${sparkSvg(c.cpu)}</div>
    <h3>${ic("memory", 15)} Memory (60s)</h3><div class="sparkline">${sparkSvg(c.mem, "purple")}</div></div>`;
}
/* Shared spark geometry: both the static sparkSvg and the in-place
   updateSparks() hot path derive identical points from the same ring. */
const sparkPoints = (data) => {
  const values = data.map(v => typeof v === "number" && Number.isFinite(v) ? v : 0);
  const w = 600, h = 40;
  const max = Math.max(...values, 1);
  const divisor = Math.max(values.length - 1, 1);
  const pts = values.map((v, i) => `${(i / divisor) * w},${h - (v / max) * (h - 4) - 2}`).join(" ");
  return { pts, w, h };
};
function sparkSvg(data, color = "accent", attrs = "") {
  if (!data.length) return "<div class='muted'>—</div>";
  const { pts, w, h } = sparkPoints(data);
  return `<svg${attrs} viewBox="0 0 ${w} ${h}" preserveAspectRatio="none"><polygon class="fill" points="0,${h} ${pts} ${w},${h}"/><polyline class="stroke-${color}" points="${pts}" fill="none" stroke-width="1.6"/></svg>`;
}

/* ---------- workspace metrics ---------- */
const METRIC_WINDOWS = [["1h", "Last hour"], ["6h", "6 hours"], ["24h", "24 hours"], ["7d", "7 days"]];
function renderMetrics(id) {
  const body = $("#tab-body");
  body.innerHTML = `
    <div class="space-between metrics-head">
      <div class="range-select" role="group" aria-label="Time range">
        ${METRIC_WINDOWS.map(([w, l]) => `<button class="btn sm ${w === "1h" ? "primary" : ""}" data-w="${w}" data-act="loadMetrics" data-sid="${id}" data-win="${w}">${l}</button>`).join("")}
      </div>
      <span class="muted" id="m-updated">${t("loading")}</span>
    </div>
    <div id="metrics-body"><div class="empty">${ic("activity", 40)}<p>${t("loading")}</p></div></div>`;
  loadMetrics(id, "1h");
}
let metricsToken = 0; // bumps per loadMetrics call; only the latest selection may write
async function loadMetrics(id, win) {
  const token = ++metricsToken;
  const body = $("#metrics-body");
  if (!body) return;
  $$("#tab-body .range-select .btn").forEach(b => b.classList.toggle("primary", b.dataset.w === win));
  try {
    const r = await api(`/servers/${id}/metrics?window=${encodeURIComponent(win)}&points=120`);
    if (token !== metricsToken) return; // a newer range superseded this response
    const data = r.data || r;
    const summary = data.summary || {};
    const series = data.series || [];
    if (!series.length) {
      body.innerHTML = `<div class="empty">${ic("activity", 40)}<p>No data yet.</p></div>`;
      return;
    }
    const num = (v, d = 0) => typeof v === "number" && Number.isFinite(v) ? v : d;
    const cpu = series.map(s => num(s.cpu_percent));
    const mem = series.map(s => num(s.memory_bytes));
    const disk = series.map(s => num(s.disk_bytes));
    const rx = series.map(s => num(s.rx_bytes));
    const tx = series.map(s => num(s.tx_bytes));
    const peak = (a) => a.length ? Math.max(...a) : 0;
    $("#m-updated").textContent = `${data.points || series.length} points · window ${data.window || win}`;
    body.innerHTML = `
      <div class="server-stats metrics-summary">
        <div class="stat"><span class="st-label">CPU avg / peak</span><span class="st-val">${num(summary.cpu_avg).toFixed(1)}% / ${num(summary.cpu_peak).toFixed(1)}%</span></div>
        <div class="stat"><span class="st-label">Memory avg / peak</span><span class="st-val">${fmtBytes(summary.memory_avg)} / ${fmtBytes(summary.memory_peak)}</span></div>
        <div class="stat"><span class="st-label">Disk peak</span><span class="st-val">${fmtBytes(summary.disk_peak)}</span></div>
        <div class="stat"><span class="st-label">Net ↓ / ↑</span><span class="st-val">${fmtBytes(summary.rx_total)} / ${fmtBytes(summary.tx_total)}</span></div>
        <div class="stat"><span class="st-label">Samples</span><span class="st-val">${num(summary.samples)}</span></div>
      </div>
      <div class="metrics-grid">
        <div class="card metric-card"><div class="metric-card-head"><h3>${ic("cpu", 15)} CPU</h3><span class="muted">peak ${Math.round(peak(cpu))}%</span></div><div class="sparkline">${metricSvg([{ data: cpu, color: "accent" }], 100)}</div></div>
        <div class="card metric-card"><div class="metric-card-head"><h3>${ic("memory", 15)} Memory</h3><span class="muted">peak ${fmtBytes(peak(mem))}</span></div><div class="sparkline">${metricSvg([{ data: mem, color: "purple" }])}</div></div>
        <div class="card metric-card"><div class="metric-card-head"><h3>${ic("globe", 15)} Network</h3><span class="muted">rx ${fmtBytes(peak(rx))} · tx ${fmtBytes(peak(tx))}</span></div><div class="sparkline">${metricSvg([{ data: rx, color: "accent" }, { data: tx, color: "purple" }])}</div></div>
        <div class="card metric-card"><div class="metric-card-head"><h3>${ic("harddisk", 15)} Storage</h3><span class="muted">peak ${fmtBytes(peak(disk))}</span></div><div class="sparkline">${metricSvg([{ data: disk, color: "yellow" }])}</div></div>
      </div>`;
  } catch (e) {
    if (token !== metricsToken) return;
    body.innerHTML = `<div class="empty">${ic("xcircle", 40)}<p>${esc(e.message)}</p></div>`;
  }
}
function metricSvg(series, ceil) {
  const n = Math.max(...series.map(s => s.data.length));
  if (!n) return "<div class='muted'>—</div>";
  const w = 600, h = 64;
  const max = ceil || Math.max(1, ...series.flatMap(s => s.data));
  const pts = (d) => d.map((v, i) => `${(i / Math.max(d.length - 1, 1)) * w},${h - (v / max) * (h - 6) - 3}`).join(" ");
  let out = `<svg viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">`;
  series.forEach((s, idx) => {
    const p = pts(s.data);
    if (idx === 0) out += `<polygon class="fill" points="0,${h} ${p} ${w},${h}"/>`;
    out += `<polyline class="stroke-${s.color}" points="${p}" fill="none" stroke-width="1.6"/>`;
  });
  return out + "</svg>";
}

/* ============================================================
   PROFILE
   ============================================================ */
function renderProfile() {
  const u = state.user;
  document.getElementById("app").innerHTML = shell("profile", t("profile"), `
    <div class="grid cols-2">
      <div class="card"><h3>${ic("profile", 15)} Account</h3>
        <div class="row mb-16"><div class="avatar lg">${esc(u.username[0] || "?").toUpperCase()}</div>
          <div><b class="p-name">${esc(u.username)}</b><div class="muted">${u.root_admin ? "Administrator" : "Member"}</div></div></div>
        <div class="field"><label>Email</label><div class="field-input">${ic("send", 14)}<input id="p-email" value="${esc(u.email)}"></div></div>
        <div class="field"><label>About</label><textarea id="p-about" rows="2">${esc(u.about || "")}</textarea></div>
        <div class="field"><label>Language</label><select id="p-lang"><option value="en" ${u.language === "en" ? "selected" : ""}>English</option><option value="id" ${u.language === "id" ? "selected" : ""}>Bahasa Indonesia</option></select></div>
        <button class="btn primary" data-act="saveProfile">${ic("save", 14)}<span>${t("save")}</span></button>
      </div>
      <div>
        <div class="card"><h3>${ic("lock", 15)} ${t("password")}</h3>
          <div class="field"><label>Current</label><div class="field-input">${ic("lock", 14)}<input type="password" id="p-cur" autocomplete="current-password"></div></div>
          <div class="field"><label>New</label><div class="field-input">${ic("key", 14)}<input type="password" id="p-new" autocomplete="new-password"></div></div>
          <button class="btn primary" data-act="savePass">Change</button>
        </div>
        <div class="card"><h3>${ic("shield", 15)} ${t("twofa")}</h3>
          <p class="muted mb-10">${u.twofa_enabled ? "Enabled" : "Disabled"}</p>
          ${u.twofa_enabled ? `
          <div class="row gap-8">
            <button class="btn" data-act="regenerateRecovery" title="${esc(t("recovery_codes"))}">${ic("refresh_ccw", 14)}<span>${t("regenerate")}</span></button>
            <button class="btn danger-ghost" data-act="disable2faPrompt">${ic("shield", 14)}<span>${t("disable_2fa")}</span></button>
          </div>` : `<button class="btn" data-act="setup2fa">${ic("shield", 14)}<span>${t("enable_2fa")}</span></button>`}
        </div>
      </div>
    </div>`);
}
async function saveProfile() {
  try {
    const u = await api("/profile", {
      method: "POST",
      body: JSON.stringify({
        email: $("#p-email").value,
        about: $("#p-about").value,
        language: $("#p-lang").value,
        theme: themePref(),
      }),
    });
    state.user = u;
    state.lang = u.language || "en";
    applyTheme();
    document.documentElement.lang = state.lang;
    toast(t("saved"), "success");
  } catch (e) {
    toast(e.message, "error");
  }
}
async function savePass() {
  try { await api("/password", { method: "POST", body: JSON.stringify({ current: $("#p-cur").value, new: $("#p-new").value }) }); toast(t("t_pass_changed"), "success"); $("#p-cur").value = ""; $("#p-new").value = ""; }
  catch (e) { toast(e.message, "error"); }
}
async function setup2fa() {
  try {
    const res = await api("/2fa/setup");
    const modal = document.createElement("div");
    modal.className = "modal";
    modal.innerHTML = `<div class="modal-card">
      <div class="modal-head"><b>${ic("shield", 15)} ${t("enable_2fa")}</b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div>
      <div class="modal-center">
        <img class="totp-qr" src="data:image/png;base64,${res.qr_b64}" alt="Authenticator QR code">
        <p class="muted m-10-0">${t("twofa_scan")}</p>
        <p class="muted">${t("twofa_secret")}: <code>${esc(res.secret)}</code></p>
      </div>
      <div class="field"><label>${t("totp_code")}</label><input id="2fa-code" inputmode="numeric" maxlength="6"></div>
      <div class="modal-foot"><button class="btn primary" data-act="confirm2fa" data-secret="${esc(res.secret)}">${t("verify")} →</button></div>
    </div>`;
    document.body.appendChild(modal);
  } catch (e) { toast(e.message, "error"); }
}
async function confirm2fa(secret, btn) {
  try {
    const res = await api("/2fa/confirm", { method: "POST", body: JSON.stringify({ secret, code: $("#2fa-code").value }) });
    toast("2FA enabled", "success");
    const m = btn ? btn.closest(".modal") : null;
    (m || $(".modal"))?.remove();
    renderProfile();
    if (Array.isArray(res.recovery_codes) && res.recovery_codes.length) showRecoveryCodes(res.recovery_codes);
  } catch (e) { toast(e.message, "error"); }
}

/* Recovery-code set shown exactly once: copy/download to keep, then
   acknowledge. The plaintext exists only in this modal. */
function showRecoveryCodes(codes) {
  const list = codes.map((c) => `<li class="recovery-code">${esc(c.slice(0, 5))}-${esc(c.slice(5))}</li>`).join("");
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.setAttribute("role", "dialog");
  modal.setAttribute("aria-modal", "true");
  modal.setAttribute("aria-labelledby", "rec-title");
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b id="rec-title">${ic("shield", 15)} ${t("recovery_codes")}</b><button class="icon-btn" data-act="closeModal" aria-label="${esc(t("close"))}">${ic("x", 16)}</button></div>
    <div class="modal-pad">
      <p class="muted">${esc(t("recovery_codes_hint"))}</p>
      <ul class="recovery-list" data-codes="${esc(codes.join(","))}">${list}</ul>
      <div class="row gap-8 mt-10">
        <button class="btn" data-act="recoveryCopy">${ic("copy", 14)}<span>${t("copy")}</span></button>
        <button class="btn" data-act="recoveryDownload">${ic("download", 14)}<span>${t("download")}</span></button>
      </div>
    </div>
    <div class="modal-foot"><button class="btn primary" data-act="closeModal">${t("recovery_confirm")}</button></div>
  </div>`;
  document.body.appendChild(modal);
}

async function recoveryCopy(el) {
  const codes = el.closest(".modal-card").querySelector(".recovery-list").dataset.codes;
  const p = navigator.clipboard?.writeText(codes.split(",").join("\n")); if (p) p.catch(() => {});
  toast(t("copy") + " ✓", "success");
}

async function recoveryDownload(el) {
  const codes = el.closest(".modal-card").querySelector(".recovery-list").dataset.codes;
  const blob = new Blob([`VoltPanel recovery codes — ${new Date().toISOString()}\n\n` + codes.split(",").join("\n") + "\n"], { type: "text/plain" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = "voltpanel-recovery-codes.txt";
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 5000);
}

/* Rotate the recovery-code set: re-auth with password + live TOTP, then the
   old codes are revoked and the new set is shown once. */
async function regenerateRecovery() {
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.setAttribute("role", "dialog");
  modal.setAttribute("aria-modal", "true");
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("refresh_ccw", 15)} ${t("regenerate")} — ${t("recovery_codes")}</b><button class="icon-btn" data-act="closeModal" aria-label="${esc(t("close"))}">${ic("x", 16)}</button></div>
    <div class="modal-pad">
      <p class="muted">${esc(t("rec_reauth_sub"))}</p>
      <div class="field"><label>${t("password")}</label><div class="field-input">${ic("lock", 14)}<input type="password" id="rr-pass" autocomplete="current-password"></div></div>
      <div class="field"><label>${t("totp_code")}</label><input id="rr-code" inputmode="numeric" maxlength="6" autocomplete="one-time-code"></div>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="regenerateRecoveryDo">${t("regenerate")}</button></div>
  </div>`;
  document.body.appendChild(modal);
  $("#rr-pass").focus();
}

async function regenerateRecoveryDo(btn) {
  try {
    const res = await api("/2fa/recovery/regenerate", {
      method: "POST",
      body: JSON.stringify({ password: $("#rr-pass").value, code: $("#rr-code").value }),
    });
    const m = btn ? btn.closest(".modal") : null;
    (m || $(".modal"))?.remove();
    showRecoveryCodes(res.recovery_codes);
    toast(t("recovery_saved"), "success");
  } catch (e) { toast(e.message, "error"); }
}

/* Disabling 2FA requires the live TOTP code; recovery codes are deleted. */
async function disable2faPrompt() {
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.setAttribute("role", "dialog");
  modal.setAttribute("aria-modal", "true");
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("shield", 15)} ${t("disable_2fa")}</b><button class="icon-btn" data-act="closeModal" aria-label="${esc(t("close"))}">${ic("x", 16)}</button></div>
    <div class="modal-pad">
      <p class="muted">${t("disable_2fa_body")}</p>
      <div class="field"><label>${t("totp_code")}</label><input id="d2fa-code" inputmode="numeric" maxlength="6" autocomplete="one-time-code"></div>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="disable2faDo">${t("disable_2fa")}</button></div>
  </div>`;
  document.body.appendChild(modal);
  $("#d2fa-code").focus();
}

async function disable2faDo(btn) {
  try {
    await api("/2fa/disable", { method: "POST", body: JSON.stringify({ code: $("#d2fa-code").value }) });
    const m = btn ? btn.closest(".modal") : null;
    (m || $(".modal"))?.remove();
    const u = await api("/me");
    state.user = u;
    toast("2FA disabled", "success");
    renderProfile();
  } catch (e) { toast(e.message, "error"); }
}

/* ============================================================
   SETTINGS
   ============================================================ */
/* ---------- scoped API keys + webhooks ---------- */
const CAPS = [
  ["control.start", "Start the workload"], ["control.stop", "Gracefully stop the workload"], ["control.restart", "Restart the workload"], ["control.kill", "Force-kill the workload"],
  ["console.read", "Read console output and logs"], ["console.write", "Send commands to the console"],
  ["files.read", "Browse and download files"], ["files.write", "Create, edit and delete files"],
  ["backups.read", "List and download backups"], ["backups.write", "Create, restore and delete backups"],
  ["schedule.read", "View schedules"], ["schedule.write", "Create and modify schedules"],
  ["database.read", "Read embedded databases"], ["database.write", "Modify embedded databases"],
  ["startup.update", "Change launch inputs"], ["startup.install", "Run the blueprint install step"], ["startup.secrets", "Reveal hidden launch inputs"],
  ["subusers.read", "View team members"], ["subusers.write", "Manage team members"],
];
/* Capability ids shown verbatim would leak internal names — prefer the
   friendly description from CAPS (i18n has no cap-label table). */
const capLabel = (c) => { const hit = CAPS.find(([id]) => id === c); return hit ? hit[1] : c; };
const WH_EVENTS = ["server.start", "server.stop", "server.crash", "server.install", "backup.complete", "backup.failed", "schedule.run", "site.updated"];
const WH_PILL = { delivered: "success", failed: "error", pending: "info" };

async function renderSettings() {
  const webhooks = state.user.root_admin ? `<div class="card"><div class="card-head"><h3>${ic("send", 15)} ${t("webhooks")} <span class="badge">admin</span></h3><button class="btn primary sm" data-act="whNew">${ic("plus", 14)}<span>${t("new_webhook")}</span></button></div><div id="wh-list"><div class="empty">${ic("send", 40)}<p>${t("loading")}</p></div></div></div>` : "";
  const notifCard = state.user.root_admin ? `<div class="card"><div class="card-head"><h3>${ic("bell", 15)} ${t("notifications")}</h3><button class="btn sm ghost" data-act="notifClear">${ic("trash", 12)}<span>${t("notif_clear_all")}</span></button></div><div id="notif-list"><div class="empty">${ic("bell", 40)}<p>${t("loading")}</p></div></div></div>` : "";
  document.getElementById("app").innerHTML = shell("settings", t("settings"), `
    <div class="grid cols-2">
      <div class="card"><div class="card-head"><h3>${ic("key", 15)} ${t("api_keys")}</h3><button class="btn primary sm" data-act="keyNew">${ic("plus", 14)}<span>${t("create")}</span></button></div><div id="keys-list"><div class="empty">${ic("key", 40)}<p>${t("loading")}</p></div></div></div>
      ${notifCard}
    </div>${webhooks}`);
  await Promise.all([keyLoad(), state.user.root_admin ? notifLoad() : Promise.resolve(), state.user.root_admin ? whLoad() : Promise.resolve()]);
}

function keyNew() {
  const modal = document.createElement("div"); modal.className = "modal";
  modal.innerHTML = `<div class="modal-card big"><div class="modal-head"><b>${ic("key", 15)} New Access Token</b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div>
    <div class="modal-pad">
      <div class="field"><label>Name</label><input id="k-name" placeholder="ci-deploy" autocomplete="off"></div>
      <div class="field"><label>Capabilities</label>
        <label class="check-row mb-8"><input type="checkbox" id="k-all"><span class="check-box">${ic("check", 13, 2.4)}</span><span>Full access (all capabilities)</span></label>
        <div class="cap-grid">${CAPS.map(([c, d]) => `<label class="check-row" title="${esc(d)}"><input type="checkbox" value="${c}"><span class="check-box">${ic("check", 13, 2.4)}</span><span>${esc(capLabel(c))}</span></label>`).join("")}</div>
      </div>
      <div class="field"><label>Server access</label><select id="k-server"><option value="">All servers</option></select></div>
      <div class="field"><label>Expiry</label><select id="k-ttl"><option value="">Never expires</option><option value="7">7 days</option><option value="30">30 days</option><option value="90">90 days</option><option value="365">1 year</option></select></div>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="keyCreate">${ic("plus", 14)}<span>${t("create")}</span></button></div></div>`;
  document.body.appendChild(modal);
  $("#k-all").addEventListener("change", (e) => { const on = e.target.checked; $$(".cap-grid input").forEach((cb) => { cb.checked = on; }); });
  api("/servers").then((r) => { const sel = $("#k-server"); (r.data || []).forEach((s) => { const o = document.createElement("option"); o.value = s.id; o.textContent = `${s.name} (#${s.id})`; sel.appendChild(o); }); }).catch(() => {});
  $("#k-name").focus();
}
async function keyCreate(btn) {
  const name = $("#k-name").value.trim(); if (!name) { toast(t("e_required_name"), "warn"); return; }
  if (!$("#k-all").checked && !$$(".cap-grid input:checked").length) { toast(t("e_pick_cap"), "warn"); return; }
  const capabilities = $("#k-all").checked ? ["*"] : $$(".cap-grid input:checked").map((cb) => cb.value);
  const server_ids = $("#k-server").value ? [+$("#k-server").value] : [];
  const ttl_days = $("#k-ttl").value ? +$("#k-ttl").value : null;
  try {
    const res = await api("/keys", { method: "POST", body: JSON.stringify({ name, capabilities, server_ids, ttl_days }) });
    const m = btn ? btn.closest(".modal") : null;
    (m || $(".modal"))?.remove(); keyLoad();
    const modal = document.createElement("div"); modal.className = "modal";
    modal.innerHTML = `<div class="modal-card"><div class="modal-head"><b>${ic("key", 15)} ${t("token_created")}</b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div>
      <div class="modal-pad-v"><p class="muted">${t("token_once")}</p><div class="code-block" id="k-token">${esc(res.token)}</div><button class="btn primary block" data-act="copyText" data-text="${esc(res.token)}">${ic("copy", 14)}<span>${t("copy_token")}</span></button></div>
      <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("done")}</button></div></div>`;
    document.body.appendChild(modal);
  } catch (e) { toast(e.message, "error"); }
}
async function keyLoad() {
  try {
    const res = await api("/keys");
    const keys = res.data || [];
    const el = $("#keys-list"); if (!el) return;
    el.innerHTML = keys.length ? `<div class="file-list">${keys.map((k) => {
      const scope = k.capabilities.includes("*") ? `<span class="badge accent">full access</span>` : k.capabilities.length ? k.capabilities.map((c) => `<span class="badge">${esc(capLabel(c))}</span>`).join("") : `<span class="badge">no scope</span>`;
      const servers = k.server_ids.length ? `servers ${k.server_ids.map((i) => `#${i}`).join(", ")}` : "all servers";
      return `<div class="cred-row${k.revoked ? " revoked" : ""}">
        <span class="cred-ico">${ic("key", 16)}</span>
        <div class="cred-main">
          <div class="cred-title"><b>${esc(k.name)}</b>${k.revoked ? `<span class="pill error plain">revoked</span>` : `<span class="pill running"><i></i>active</span>`}</div>
          <div class="cred-meta"><span class="badge">${esc(servers)}</span><span class="badge">expires ${k.expires_at ? esc(fmtDate(k.expires_at)) : "never"}</span>${scope}</div>
        </div>
        <span class="f-actions">${k.revoked ? "" : `<button class="icon-btn sm" title="Revoke key" data-act="keyRevoke" data-id="${k.id}">${ic("pause", 14)}</button>`}<button class="icon-btn sm danger" title="${t("delete")}" data-act="keyDel" data-id="${k.id}">${ic("trash", 14)}</button></span>
        <div class="cred-side"><span class="muted">last used ${k.last_used ? esc(fmtDate(k.last_used)) : "never"}</span></div>
      </div>`; }).join("")}</div>` : emptyState("key", t("es_key_t"), t("es_key_h"), { act: "keyNew", actLabel: t("create") });
  } catch (e) {
    const el = $("#keys-list");
    if (el) el.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_keys")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="keyRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
async function keyRevoke(id) {
  if (!await vpConfirm(t("confirm_revoke_key"))) return;
  try { await api(`/keys/${id}/revoke`, { method: "POST" }); toast(t("t_key_revoked"), "success"); keyLoad(); } catch (e) { toast(e.message, "error"); }
}
async function keyDel(id) { if (!await vpConfirm(t("confirm_delete"))) return; try { await api(`/keys/${id}`, { method: "DELETE" }); keyLoad(); } catch (e) { toast(e.message, "error"); } }

function whForm(w) {
  const modal = document.createElement("div"); modal.className = "modal";
  const evs = (w?.events || ["*"]).includes("*") ? ["*"] : (w?.events || ["*"]);
  const serverVal = w?.server_id ?? "";
  modal.innerHTML = `<div class="modal-card"><div class="modal-head"><b>${ic("send", 15)} ${w ? "Edit webhook" : "New webhook"}</b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div>
    <div class="modal-pad">
      <div class="field"><label>Name</label><input id="wh-name" value="${esc(w?.name || "")}" placeholder="deploy-notify"></div>
      <div class="field"><label>URL</label><input id="wh-url" value="${esc(w?.url || "")}" placeholder="https://hooks.example.com/voltpanel"></div>
      <div class="field"><label>Events</label>
        <label class="check-row mb-8"><input type="checkbox" id="wh-all"${evs.includes("*") ? " checked" : ""}><span class="check-box">${ic("check", 13, 2.4)}</span><span>All events</span></label>
        <div class="cap-grid">${WH_EVENTS.map((e) => `<label class="check-row"><input type="checkbox" value="${e}"${evs.includes(e) ? " checked" : ""}><span class="check-box">${ic("check", 13, 2.4)}</span><span>${esc(e)}</span></label>`).join("")}</div>
      </div>
      <div class="field"><label>Server</label><select id="wh-server"><option value="">All servers (global)</option></select></div>
      <div class="field"><label>Signing secret</label><input id="wh-secret" placeholder="${w ? "leave blank to keep current" : "leave blank to auto-generate"}" autocomplete="off"><small class="muted">Signs the X-Volt-Timestamp / X-Volt-Signature HMAC headers</small></div>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="whSave" data-id="${w?.id || 0}">${ic("save", 14)}<span>${w ? "Save changes" : t("create")}</span></button></div></div>`;
  document.body.appendChild(modal);
  $("#wh-all").addEventListener("change", (e) => { const on = e.target.checked; $$(".cap-grid input").forEach((cb) => { cb.checked = on; }); });
  api("/servers").then((r) => { const sel = $("#wh-server"); (r.data || []).forEach((s) => { const o = document.createElement("option"); o.value = s.id; o.textContent = `${s.name} (#${s.id})`; sel.appendChild(o); }); sel.value = String(serverVal); }).catch(() => {});
  $("#wh-name").focus();
}
function whNew() { whForm(null); }
function whEdit(id) { api(`/webhooks/${id}`).then((r) => whForm(r.data)).catch((e) => toast(e.message, "error")); }
async function whSave(id, btn) {
  const name = $("#wh-name").value.trim(); if (!name) { toast(t("e_required_name"), "warn"); return; }
  const url = $("#wh-url").value.trim(); if (!url) { toast(t("e_required_url"), "warn"); return; }
  const events = $("#wh-all").checked ? ["*"] : $$(".cap-grid input:checked").map((cb) => cb.value);
  const server_id = $("#wh-server").value ? +$("#wh-server").value : null;
  const body = { name, url, events, server_id };
  const secret = $("#wh-secret").value.trim();
  if (secret) body.secret = secret;
  try {
    if (id) {
      await api(`/webhooks/${id}`, { method: "PATCH", body: JSON.stringify(body) });
      const m = btn ? btn.closest(".modal") : null;
      (m || $(".modal"))?.remove(); whLoad(); toast(t("t_wh_updated"), "success");
    } else {
      const res = await api("/webhooks", { method: "POST", body: JSON.stringify(body) });
      const m = btn ? btn.closest(".modal") : null;
      (m || $(".modal"))?.remove(); whLoad();
      const modal = document.createElement("div"); modal.className = "modal";
      modal.innerHTML = `<div class="modal-card"><div class="modal-head"><b>${ic("send", 15)} ${t("webhook_created")}</b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div>
        <div class="modal-pad-v"><p class="muted">${t("wh_secret_once")}</p><div class="code-block" id="wh-secret-out">${esc(res.data?.secret || "")}</div><button class="btn primary block" data-act="copyText" data-text="${esc(res.data?.secret || "")}">${ic("copy", 14)}<span>${t("copy_secret")}</span></button></div>
        <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("done")}</button></div></div>`;
      document.body.appendChild(modal);
    }
  } catch (e) { toast(e.message, "error"); }
}
async function whToggle(id) { try { await api(`/webhooks/${id}/toggle`, { method: "POST" }); whLoad(); } catch (e) { toast(e.message, "error"); } }
async function whTest(id) {
  try { const r = await api(`/webhooks/${id}/test`, { method: "POST" }); toast(`Test ping enqueued${r.data?.event ? " (" + r.data.event + ")" : ""}`, "success"); whLoad(); }
  catch (e) { toast(e.message, "error"); }
}
async function whDel(id) { if (!await vpConfirm("Delete this webhook? Deliveries stop immediately.")) return; try { await api(`/webhooks/${id}`, { method: "DELETE" }); whLoad(); toast(t("deleted"), "success"); } catch (e) { toast(e.message, "error"); } }
async function whDeliveries(id) {
  const modal = document.createElement("div"); modal.className = "modal";
  modal.innerHTML = `<div class="modal-card big"><div class="modal-head"><b>${ic("activity", 15)} Webhook deliveries</b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div><div id="wh-deliveries"><div class="empty">${ic("activity", 40)}<p>${t("loading")}</p></div></div></div>`;
  document.body.appendChild(modal);
  try {
    const res = await api(`/webhooks/${id}/deliveries?limit=50`);
    const rows = res.data || [];
    const box = $("#wh-deliveries");
    box.innerHTML = rows.length ? `<div class="file-list">${rows.map((d) => `<div class="cred-row">
      <span class="cred-ico">${ic("send", 15)}</span>
      <div class="cred-main">
        <div class="cred-title"><code>${esc(d.event)}</code><span class="pill ${WH_PILL[d.status] || ""}"><i></i>${esc(d.status)}</span>${d.response_code ? `<span class="badge">HTTP ${d.response_code}</span>` : ""}<span class="badge">attempt ${d.attempt}</span></div>
        <div class="cred-meta"><span class="muted">created ${fmtDate(d.created_at)}${d.delivered_at ? ` · delivered ${esc(fmtDate(d.delivered_at))}` : ""}${d.next_attempt_at ? ` · retry ${esc(new Date(d.next_attempt_at * 1000).toLocaleString())}` : ""}</span></div>
        ${d.error ? `<div class="cred-error">${esc(d.error)}</div>` : ""}
      </div></div>`).join("")}</div>` : emptyState("activity", t("es_deliv_t"), t("es_deliv_h"));
  } catch (e) { $("#wh-deliveries").innerHTML = `<div class="empty">${ic("alert", 40)}<p>${esc(e.message)}</p></div>`; }
}
async function whLoad() {
  try {
    const res = await api("/webhooks");
    const whs = res.data || [];
    const el = $("#wh-list"); if (!el) return;
    el.innerHTML = whs.length ? `<div class="file-list">${whs.map((w) => {
      const evs = w.events.includes("*") ? `<span class="badge accent">all events</span>` : w.events.map((e) => `<span class="badge">${esc(e)}</span>`).join("");
      return `<div class="cred-row">
        <span class="cred-ico">${ic("send", 16)}</span>
        <div class="cred-main">
          <div class="cred-title"><b>${esc(w.name)}</b><span class="pill ${w.enabled ? "running" : "offline"}"><i></i>${w.enabled ? "enabled" : "disabled"}</span>${w.failure_count ? `<span class="pill error plain">${w.failure_count} failures</span>` : ""}</div>
          <div class="cred-meta"><code class="cred-url">${esc(w.url)}</code>${evs}<span class="badge">${w.server_id ? `server #${w.server_id}` : "all servers"}</span>${w.last_status ? `<span class="badge">last: ${esc(w.last_status)}</span>` : ""}</div>
        </div>
        <span class="f-actions">
          <button class="icon-btn sm" title="Toggle webhook" data-act="whToggle" data-id="${w.id}">${w.enabled ? ic("pause", 14) : ic("play", 14)}</button>
          <button class="icon-btn sm" title="Send test ping" data-act="whTest" data-id="${w.id}">${ic("zap", 14)}</button>
          <button class="icon-btn sm" title="Deliveries" data-act="whDeliveries" data-id="${w.id}">${ic("activity", 14)}</button>
          <button class="icon-btn sm" title="${t("edit")}" data-act="whEdit" data-id="${w.id}">${ic("pencil", 14)}</button>
          <button class="icon-btn sm danger" title="${t("delete")}" data-act="whDel" data-id="${w.id}">${ic("trash", 14)}</button>
        </span>
      </div>`; }).join("")}</div>` : emptyState("send", t("es_wh_t"), t("es_wh_h"), { act: "whNew", actLabel: t("create") });
  } catch (e) {
    const el = $("#wh-list");
    if (el) el.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_webhooks")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="whRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
/* ---------- Notification center ---------- */
const NOTIF_CLS = { info: "info", warn: "warn", error: "error", success: "success" };
const notifCls = (l) => NOTIF_CLS[String(l ?? "")] || "info";

async function notifLoad() {
  const box = $("#notif-list");
  if (!box) return;
  try {
    const res = await api("/notifications");
    const data = res.data || {};
    const notifs = (data.entries || []).slice(-30).reverse();
    box.innerHTML = notifs.length ? `<div class="file-list">${notifs.map((n) => `<div class="file-row"><span class="pill ${notifCls(n.level)}"><i></i>${esc(n.level)}</span><b>${esc(n.title)}</b><span class="f-meta">${fmtDate(n.created_at)}</span></div>`).join("")}</div>` : emptyState("bell", t("es_notif_t"), t("es_notif_h"));
  } catch (e) {
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_notifications")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="notifRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
    toast(e.message, "error");
  }
}

/* Open one SSE feed for root admins. EventSource reconnects on its own, so
   a panel restart or dropped connection resumes the live feed without any
   polling; each reconnect re-syncs the snapshot. */
function notifInit() {
  if (!state.user?.root_admin || state.notifEs) return;
  const es = new EventSource(API + "/notifications/stream");
  state.notifEs = es;
  es.onopen = () => notifRefresh();
  es.addEventListener("notification", (ev) => {
    let n;
    try { n = JSON.parse(ev.data); } catch (e) { return; }
    state.notifs = [n, ...state.notifs].slice(0, 200);
    if (!n.read_at) state.unread++;
    updateNotifBadge();
    notifRender();
  });
  // EventSource reconnects automatically on error; nothing to do here.
}

function notifTeardown() {
  if (state.notifEs) { state.notifEs.close(); state.notifEs = null; }
  state.notifs = [];
  state.unread = 0;
}

async function notifRefresh() {
  try {
    const res = await api("/notifications");
    const data = res.data || {};
    state.notifs = (data.entries || []).slice(-200).reverse();
    state.unread = data.unread_count || 0;
  } catch (e) { /* keep the last known state; SSE will resync on reconnect */ }
  updateNotifBadge();
  notifRender();
}

function updateNotifBadge() {
  const badge = $(".notif-badge");
  if (!badge) return;
  const n = state.unread;
  badge.hidden = !n;
  badge.textContent = n > 99 ? "99+" : n;
}

function notifRender() {
  const list = $("#notif-panel-list");
  if (!list || list.closest("#notif-panel").hidden) return;
  const items = state.notifs.slice(0, 30);
  const head = $(".notif-unread");
  if (head) head.textContent = state.unread ? t("notif_unread").replace("{n}", state.unread) : t("notif_all_read");
  if (!items.length) {
    list.innerHTML = `<div class="empty notif-empty">${ic("bell", 32)}<p>${t("notif_empty")}</p></div>`;
    return;
  }
  list.innerHTML = items.map((n) => `<button class="notif-item${n.read_at ? "" : " unread"}" data-act="notifItem" data-id="${n.id}" data-link="${esc(n.link || "")}">
    <span class="pill ${notifCls(n.level)} plain"><i></i>${esc(n.level)}</span>
    <span class="notif-text"><b>${esc(n.title)}</b><span class="muted">${esc(n.message)}</span></span>
    <span class="notif-time">${fmtDate(n.created_at)}</span>
    ${n.link ? ic("chevron_right", 13) : ""}
  </button>`).join("");
}

function notifToggle() {
  const panel = $("#notif-panel");
  if (!panel) return;
  const opening = panel.hidden;
  panel.hidden = !opening;
  $(".notif-bell")?.setAttribute("aria-expanded", String(opening));
  if (opening) {
    notifRender();
    panel.querySelector("button, a, [tabindex]")?.focus?.();
  }
}

async function notifItem(el) {
  const id = +el.dataset.id;
  const link = el.dataset.link;
  const n = state.notifs.find((x) => x.id === id);
  if (n && !n.read_at) {
    n.read_at = new Date().toISOString();
    state.unread = Math.max(0, state.unread - 1);
    updateNotifBadge();
    notifRender();
    api(`/notifications/${id}/read`, { method: "POST" }).catch(() => {});
  }
  if (link) {
    const panel = $("#notif-panel");
    if (panel) { panel.hidden = true; $(".notif-bell")?.setAttribute("aria-expanded", "false"); }
    location.hash = link;
  }
}

async function notifClear() {
  try {
    await api("/notifications/clear", { method: "POST" });
    state.notifs = [];
    state.unread = 0;
    updateNotifBadge();
    notifRender();
    notifLoad();
    toast(t("notif_cleared"), "success");
  } catch (e) { toast(e.message, "error"); }
}

/* Esc closes the drawer; a click outside the bell/drawer closes it too. */
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  const panel = $("#notif-panel");
  if (panel && !panel.hidden) {
    panel.hidden = true;
    $(".notif-bell")?.setAttribute("aria-expanded", "false");
  }
});
document.addEventListener("click", (e) => {
  const panel = $("#notif-panel");
  if (panel && !panel.hidden && !e.target.closest(".notif-wrap")) {
    panel.hidden = true;
    $(".notif-bell")?.setAttribute("aria-expanded", "false");
  }
}, true);

/* ============================================================
   ADMIN
   ============================================================ */
function renderAdmin(tab) {
  const hp = location.hash.slice(1).split("/").filter(Boolean);
  const detailId = hp[2] ? +hp[2].replace(/\D/g, "") : 0;
  /* squad_get is authenticated-user-scoped server-side, so the squad DETAIL
     is reachable by any signed-in user (member, manager, or non-member);
     every other Control Center surface stays root-only. The squad LIST
     (/api/admin/squads) remains admin-only — non-root users deep-link. */
  if (!state.user?.root_admin) {
    if (tab === "squads" && detailId) {
      document.getElementById("app").innerHTML = shell("squads", t("squads"), `<div id="admin-body"><div class="empty">${ic("users", 40)}<p>${t("loading")}</p></div></div>`);
      return adminSquadDetail(detailId);
    }
    toast(t("control_center_only"), "error"); renderDashboard(); return;
  }
  const active = tab === "nodes" ? "fabric" : tab === "blueprints" ? "blueprints" : tab === "system" ? "observatory" : tab === "squads" ? "squads" : "workspaces";
  document.getElementById("app").innerHTML = shell(active, t("control_center"), `<div class="tabs">
    <a href="#/admin/servers" class="${tab === "servers" ? "active" : ""}">${ic("server", 14)} ${t("servers")}</a>
    <a href="#/admin/users" class="${tab === "users" ? "active" : ""}">${ic("users", 14)} ${t("users")}</a>
    <a href="#/admin/squads" class="${tab === "squads" ? "active" : ""}">${ic("users", 14)} ${t("squads")}</a>
    <a href="#/admin/blueprints" class="${tab === "blueprints" ? "active" : ""}">${ic("box", 14)} ${t("blueprints")}</a>
    <a href="#/admin/nodes" class="${tab === "nodes" ? "active" : ""}">${ic("globe", 14)} ${t("node")}</a>
    <a href="#/admin/system" class="${tab === "system" ? "active" : ""}">${ic("gauge", 14)} ${t("system")}</a>
  </div><div id="admin-body"><div class="empty">${ic("shield", 40)}<p>${t("loading")}</p></div></div>`);
  const render = { servers: adminServers, users: adminUsers, squads: adminSquads, blueprints: adminBlueprints, nodes: adminNodes, system: adminSystem };
  if (tab === "users" && detailId) return adminUserDetail(detailId);
  if (tab === "squads" && detailId) return adminSquadDetail(detailId);
  if (tab === "nodes" && detailId) return adminNodeDetail(detailId);
  (render[tab] || adminServers)();
}

/* Fleet workspaces admin table — row selection + floating bulk bar.
   Bulk verbs reuse the per-server power / suspend endpoints; the run is
   bounded to 4 concurrent so a large fleet never hammers the panel. Per-row
   busy/ok/fail marks render live; no bulk delete this iteration. */
let fleetSort = { key: "name", dir: 1 };
let fleetRows = [];
const fleetSel = new Set();
const fleetBusy = new Set();
let fleetRunning = false;

async function adminServers() {
  const bulkBtn = (key, icon, label, danger) => `<button class="btn sm${danger ? " danger" : ""}" data-act="fleetBulk" data-key="${key}"${fleetRunning ? " disabled" : ""}>${ic(icon, 13)}<span>${esc(label)}</span></button>`;
  $("#admin-body").innerHTML = `<div class="card"><div class="card-head"><h3>${t("all_servers")}</h3><button class="btn primary sm" data-act="adminNewServer">${ic("plus", 14)}<span>${t("create_server")}</span></button></div><div id="a-servers"></div></div>
    <div class="bulk-bar" id="bulk-bar" hidden>
      <span class="bulk-count" id="bulk-count"></span>
      <div class="bulk-actions">
        ${bulkBtn("start", "play", t("bulk_start"))}
        ${bulkBtn("stop", "stop", t("bulk_stop"), true)}
        ${bulkBtn("restart", "refresh", t("bulk_restart"))}
        ${bulkBtn("suspend", "pause", t("bulk_suspend"))}
        ${bulkBtn("unsuspend", "play", t("bulk_unsuspend"))}
      </div>
      <button class="icon-btn sm" data-act="fleetSelClear" title="${esc(t("sel_clear"))}" aria-label="${esc(t("sel_clear"))}">${ic("x", 15)}</button>
    </div>`;
  try {
    const res = await api("/servers/all");
    fleetRenderTable(res.data || []);
  } catch (e) {
    const box = $("#a-servers");
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_servers")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminServersRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
  fleetUpdateBar();
}

function fleetRenderTable(servers) {
  const box = $("#a-servers");
  if (!box) return;
  fleetRows = servers;
  fleetSel.forEach((id) => { if (!servers.some((s) => s.id === id)) fleetSel.delete(id); });
  const dir = fleetSort.dir;
  const sorted = servers.slice().sort((a, b) => {
    if (fleetSort.key === "memory_mb" || fleetSort.key === "disk_mb") return dir * ((+a[fleetSort.key] || 0) - (+b[fleetSort.key] || 0));
    return dir * String(a[fleetSort.key] ?? "").localeCompare(String(b[fleetSort.key] ?? ""));
  });
  const arrow = (key) => fleetSort.key === key ? (dir > 0 ? " ↑" : " ↓") : "";
  const aria = (key) => fleetSort.key === key ? (dir > 0 ? "ascending" : "descending") : "none";
  const th = (key, label) => `<th data-act="fleetSort" data-key="${key}" tabindex="0" aria-sort="${aria(key)}" title="${esc(t(key === "status" ? "status" : key === "memory_mb" ? "ram" : key === "disk_mb" ? "disk" : "name"))}">${label}${arrow(key)}</th>`;
  box.innerHTML = `<div class="tbl-wrap"><table class="tbl fleet-tbl"><thead><tr>
    <th class="col-id">ID</th>
    <th class="col-check"><label class="f-check"><input type="checkbox" id="fleet-all" data-act="fleetSelAll" data-act-change aria-label="${esc(t("sel_all"))}"><span></span></label></th>
    ${th("name", t("name"))}${th("status", t("status"))}${th("memory_mb", "RAM")}${th("disk_mb", "Disk")}
    <th class="col-owner">${t("owner")}</th><th class="col-actions"></th>
  </tr></thead><tbody>` +
    sorted.map((s) => `<tr class="${fleetSel.has(s.id) ? "fleet-sel" : ""}${fleetBusy.has(s.id) ? " fleet-busy" : ""}">
      <td class="col-id">${s.id}</td>
      <td class="col-check"><label class="f-check"><input type="checkbox" data-act="fleetToggleSel" data-act-change data-id="${s.id}" aria-label="${esc(s.name)}"${fleetSel.has(s.id) ? " checked" : ""}${fleetBusy.has(s.id) ? " disabled" : ""}><span></span></label></td>
      <td><a href="#/server/${s.id}" class="link-strong">${esc(s.name)}</a></td>
      <td><span class="pill ${statusCls(s.status)}"><i></i>${esc(s.status)}</span></td>
      <td>${s.memory_mb} MB</td><td>${s.disk_mb} MB</td>
      <td class="col-owner">#${esc(s.user_id)}</td>
      <td class="col-actions" data-row="${s.id}"><span class="fleet-result" hidden></span>
        <div class="actions"><button class="icon-btn sm" data-act="adminToggleSuspend" data-id="${s.id}" data-on="${s.suspended ? "0" : "1"}"${fleetBusy.has(s.id) ? " disabled" : ""}>${s.suspended ? ic("play", 15) : ic("pause", 15)}</button><button class="icon-btn sm danger" data-act="adminDelServer" data-id="${s.id}" data-name="${esc(s.name)}"${fleetBusy.has(s.id) ? " disabled" : ""}>${ic("trash", 15)}</button></div>
      </td>
    </tr>`).join("") + `</tbody></table></div>`;
  const all = $("#fleet-all");
  if (all) { all.checked = servers.length > 0 && fleetSel.size === servers.length; all.indeterminate = fleetSel.size > 0 && fleetSel.size < servers.length; }
  /* Sortable headers are keyboard-activatable: Enter/Space on a focused
     header toggles the same sort the click applies. */
  $$(".fleet-tbl th[data-act='fleetSort']", box).forEach((th) => {
    th.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") { e.preventDefault(); fleetSortToggle(th.dataset.key); }
    });
  });
  fleetUpdateBar();
}

function fleetSetRowResult(id, state) {
  const td = $(`[data-row="${id}"]`);
  const box = td?.querySelector(".fleet-result");
  if (!box) return;
  if (state === "busy") { box.hidden = false; box.className = "fleet-result busy"; box.innerHTML = ic("activity", 14); }
  else if (state === "ok") { box.hidden = false; box.className = "fleet-result ok"; box.innerHTML = ic("check", 14); }
  else if (state === "fail") { box.hidden = false; box.className = "fleet-result fail"; box.innerHTML = ic("xcircle", 14); }
}

function fleetUpdateBar() {
  const bar = $("#bulk-bar");
  if (!bar) return;
  const n = fleetSel.size;
  $("#bulk-count").textContent = t("sel_n").replace("{n}", n);
  bar.hidden = n === 0;
  $$("#bulk-bar [data-act='fleetBulk']").forEach((b) => { b.disabled = fleetRunning || n === 0; });
}

function fleetToggleSel(cb) {
  const id = +cb.dataset.id;
  if (cb.checked) fleetSel.add(id); else fleetSel.delete(id);
  cb.closest("tr")?.classList.toggle("fleet-sel", cb.checked);
  const all = $("#fleet-all");
  if (all) { all.checked = fleetSel.size > 0 && fleetSel.size === fleetRows.length; all.indeterminate = fleetSel.size > 0 && fleetSel.size < fleetRows.length; }
  fleetUpdateBar();
}

function fleetSelAll(cb) {
  if (cb.checked) fleetRows.forEach((s) => fleetSel.add(s.id));
  else fleetSel.clear();
  $$(".fleet-tbl tbody input[data-act='fleetToggleSel']").forEach((c) => { c.checked = cb.checked; c.closest("tr")?.classList.toggle("fleet-sel", cb.checked); });
  fleetUpdateBar();
}

function fleetSelClear() {
  fleetSel.clear();
  $$(".fleet-tbl tbody input[data-act='fleetToggleSel']").forEach((c) => { c.checked = false; c.closest("tr")?.classList.toggle("fleet-sel", false); });
  const all = $("#fleet-all"); if (all) { all.checked = false; all.indeterminate = false; }
  fleetUpdateBar();
}

function fleetSortToggle(key) {
  if (fleetSort.key === key) fleetSort.dir *= -1;
  else { fleetSort.key = key; fleetSort.dir = 1; }
  fleetRenderTable(fleetRows);
}

/* Bounded-concurrency bulk runner: at most 4 in flight. `stop` is
   destructive (running processes die) so it asks first; the other verbs are
   reversible/stateful and run immediately. */
async function fleetBulk(verb) {
  if (fleetRunning) return;
  const ids = [...fleetSel];
  if (!ids.length) return;
  if (verb === "stop" && !await vpConfirm(t("bulk_confirm_stop").replace("{n}", ids.length), t("bulk_stop"))) return;
  fleetRunning = true;
  fleetUpdateBar();
  const queue = ids.slice();
  let ok = 0, fail = 0;
  const worker = async () => {
    while (queue.length) {
      const id = queue.shift();
      fleetBusy.add(id);
      fleetSetRowResult(id, "busy");
      try {
        if (verb === "suspend" || verb === "unsuspend") await api(`/servers/${id}/${verb}`, { method: "POST" });
        else await api(`/servers/${id}/power`, { method: "POST", body: JSON.stringify({ action: verb }) });
        ok++; fleetSetRowResult(id, "ok");
      } catch (e) { fail++; fleetSetRowResult(id, "fail"); }
      finally { fleetBusy.delete(id); }
    }
  };
  await Promise.all(Array.from({ length: Math.min(4, ids.length) }, worker));
  toast(t("bulk_done").replace("{ok}", ok).replace("{fail}", fail), fail ? "warn" : "success");
  fleetRunning = false;
  fleetSel.clear();
  fleetUpdateBar();
  await new Promise((r) => setTimeout(r, 900)); // let the per-row marks be read
  if (document.getElementById("admin-body")) adminServers();
}
async function adminToggleSuspend(id, on) { try { await api(`/servers/${id}/${on ? "suspend" : "unsuspend"}`, { method: "POST" }); adminServers(); } catch (e) { toast(e.message, "error"); } }
async function adminDelServer(id, name) {
  if (!await vpDestroy({
    kind: "server", target: name || `server-${id}`,
    consequences: ["All files, databases and backups are erased", "Allocated ports are released", "This cannot be undone"],
  })) return;
  try { await api(`/servers/${id}`, { method: "DELETE" }); adminServers(); toast(t("deleted"), "success"); } catch (e) { toast(e.message, "error"); }
}
/* ============================================================
   VoltSpec composer — split-pane provisioner.
   Left pane: blueprint + typed variable inputs; right pane:
   live launch preview (rendered startup command, resource
   summary, endpoint). Mirrors the server contract (variables +
   start_on_create in CreateServerReq) and services/blueprint.rs
   validation + quoting rules, so the preview is honest.
   ============================================================ */
let prBlueprint = null; // blueprint currently selected in the composer
function prVarInput(v, idx) {
  const k = v.kind || { type: "text" };
  const dis = v.user_editable ? "" : "disabled";
  const val = esc(v.default_value ?? "");
  if (k.type === "choice") {
    const opts = (k.options || []).map((o) => `<option value="${esc(o)}"${String(o) === String(v.default_value) ? " selected" : ""}>${esc(o)}</option>`).join("");
    return `<select id="pr-var-${idx}" data-pvar="${esc(v.env_var)}" ${dis}>${opts}</select>`;
  }
  if (k.type === "bool") {
    const on = String(v.default_value).toLowerCase() === "true";
    return `<label class="check-row"><input type="checkbox" id="pr-var-${idx}" data-pvar="${esc(v.env_var)}" data-bool="1"${on ? " checked" : ""} ${dis}><span class="check-box">${ic("check", 13, 2.4)}</span></label>`;
  }
  const attrs = [];
  let type = "text";
  if (k.type === "number") {
    type = "number";
    if (k.min !== undefined && k.min !== null) attrs.push(`min="${esc(String(k.min))}"`);
    if (k.max !== undefined && k.max !== null) attrs.push(`max="${esc(String(k.max))}"`);
  } else if (k.type === "url") {
    type = "url";
  }
  if (k.max_len) attrs.push(`maxlength="${esc(String(k.max_len))}"`);
  if (k.pattern) attrs.push(`pattern="${esc(k.pattern)}"`);
  if (v.required) attrs.push("required");
  return `<div class="field-input">${ic("pencil", 14)}<input id="pr-var-${idx}" type="${type}" data-pvar="${esc(v.env_var)}" value="${val}" ${attrs.join(" ")} ${dis}></div>`;
}
function prKindLabel(k) { return k && k.type !== "text" ? `<span class="badge">${esc(k.type)}</span>` : ""; }
/* Client-side mirror of services/blueprint.rs validate_value. */
function prValidateVar(v, value) {
  const trimmed = String(value ?? "").trim();
  if (!trimmed) return v.required ? `${v.env_var} is required` : null;
  const k = v.kind || {};
  if (k.type === "number") {
    const n = Number(trimmed);
    if (!Number.isFinite(n)) return `${v.env_var} must be a number`;
    if (k.min !== undefined && k.min !== null && n < k.min) return `${v.env_var} must be at least ${k.min}`;
    if (k.max !== undefined && k.max !== null && n > k.max) return `${v.env_var} must be at most ${k.max}`;
  } else if (k.type === "path") {
    if (k.max_len && trimmed.length > k.max_len) return `${v.env_var} is too long`;
    if (trimmed.startsWith("/")) return `${v.env_var} must be a server-relative path`;
    if (trimmed.startsWith("~")) return `${v.env_var} must not reference a home directory`;
    if (trimmed.split("/").includes("..")) return `${v.env_var} must not traverse outside the server`;
  } else if (k.type === "url") {
    let u;
    try { u = new URL(trimmed); } catch (e) { return `${v.env_var} must be an http(s) URL`; }
    if (!["http:", "https:"].includes(u.protocol) || !u.hostname) return `${v.env_var} must be an http(s) URL`;
    if (/[;|$`()<>*~"'{}[\]!^\\\s]/.test(trimmed)) return `${v.env_var} contains shell metacharacters`;
  } else if (k.type === "choice") {
    if (!(k.options || []).some((o) => String(o) === trimmed)) return `${v.env_var} must be one of: ${(k.options || []).join(", ")}`;
  } else if (k.type === "bool") {
    if (!["true", "false", "1", "0", "yes", "no", "on", "off"].includes(trimmed.toLowerCase())) return `${v.env_var} must be a boolean`;
  } else {
    if (k.max_len && trimmed.length > k.max_len) return `${v.env_var} is too long`;
    if (k.pattern) { try { if (!new RegExp(k.pattern).test(trimmed)) return `${v.env_var} does not match required pattern`; } catch (e) {} }
  }
  return null;
}
/* Mirror render_impl quoting: input.* and workspace.name are shell-quoted
   unless made only of safe characters; workspace metadata stays bare. */
function prShellQuote(value) {
  const s = String(value);
  return /^[A-Za-z0-9\-_.,:/@=+%]+$/.test(s) ? s : "'" + s.replace(/'/g, "'\\''") + "'";
}
function prRenderLaunch(template, vars, workspace, secrets) {
  const problems = [];
  const html = template.replace(/\$\{\s*([a-z]+)\.([A-Za-z0-9_]+)\s*\}/g, (m, ns, key) => {
    if (ns === "input") {
      if (secrets.has(key)) { problems.push(`secret input.${key} is env-only and never rendered`); return `<span class="pr-bad">${m}</span>`; }
      const v = vars.get(key);
      if (v === undefined || v === "") { problems.push(`input.${key} is empty`); return `<span class="pr-bad">${m}</span>`; }
      return `<span class="pr-val">${esc(prShellQuote(v))}</span>`;
    }
    if (ns === "workspace") {
      const wv = key === "name" ? prShellQuote(workspace.name) : workspace[key];
      if (wv === undefined || wv === "") return `<span class="pr-warn">(unset)</span>`;
      return `<span class="pr-val">${esc(String(wv))}</span>`;
    }
    problems.push(`unknown placeholder ${m}`);
    return `<span class="pr-bad">${m}</span>`;
  });
  return { html, problems };
}
async function adminNewServer() {
  try {
    const [usersRes, blueprintsRes, nodesRes] = await Promise.all([
      api("/admin/users"),
      api("/blueprints"),
      api("/nodes").catch(() => ({ data: [] })),
    ]);
    const users = usersRes.data || usersRes;
    const blueprints = blueprintsRes.data || blueprintsRes;
    const nodes = (nodesRes.data || nodesRes || []).filter((n) => n && n.name);
    if (!blueprints.length) { toast(t("runway_need_blueprint"), "warn"); return; }
    if (!nodes.length) { toast(t("place_need_node"), "warn"); return; }
    prBlueprint = blueprints[0];
    // Placement options: auto-fit + one per enrolled agent, each showing
    // live free-capacity so the pick is informed, not a bare name dropdown.
    const fmtMb = (mb) => (mb >= 1024 ? (mb / 1024).toFixed(mb % 1024 ? 1 : 0) + " GB" : mb + " MB");
    const nodeOption = (n) => {
      const free = t("place_free", { mem: fmtMb(Math.max(0, +n.available_memory_mb || 0)), disk: fmtMb(Math.max(0, +n.available_disk_mb || 0)) });
      const dot = n.online ? "●" : "○";
      return `<option value="node:${esc(n.name)}"${n.online ? "" : " disabled"}>${dot} ${esc(n.name)}${n.location ? " — " + esc(n.location) : ""} · ${n.online ? esc(free) : esc(t("place_node_offline"))}</option>`;
    };
    const placeOptions = `<option value="auto">${t("place_auto")}</option>`
      + nodes.map(nodeOption).join("");
    const modal = document.createElement("div"); modal.className = "modal";
    modal.innerHTML = `<div class="modal-card big provisioner-modal">
      <div class="modal-head"><b>${ic("server", 15)} ${t("create_server")} <span class="badge">VoltSpec</span></b><button class="icon-btn" data-act="closeModal" aria-label="${t("close")}">${ic("x", 16)}</button></div>
      <div class="provisioner">
        <div class="provisioner-pane">
          <div class="field"><label for="pr-name">${t("name")}</label><input id="pr-name" placeholder="my-server" autocomplete="off"></div>
          <div class="field"><label for="pr-user">${t("owner")}</label><select id="pr-user">${users.map((u) => `<option value="${u.id}">${esc(u.username)} (#${u.id})</option>`).join("")}</select></div>
          <div class="field"><label for="pr-blueprint">${t("runway_blueprint")}</label><select id="pr-blueprint">${blueprints.map((b) => `<option value="${b.id}">${esc(b.name)} — ${esc(b.category)}</option>`).join("")}</select></div>
          <div class="field"><label for="pr-place">${ic("globe", 13)} ${t("place_label")}</label><select id="pr-place">${placeOptions}</select><small id="pr-place-hint" class="muted">${t("place_hint_auto")}</small></div>
          <div class="grid cols-2 pr-auto-hints" id="pr-auto-hints" hidden>
            <div class="field"><label for="pr-tags">${t("place_tags")}</label><input id="pr-tags" placeholder="${t("place_tags_ph")}" autocomplete="off"></div>
            <div class="field"><label for="pr-loc">${t("place_location")}</label><input id="pr-loc" placeholder="${t("place_location_ph")}" autocomplete="off"></div>
          </div>
          <div id="pr-vars"></div>
          <div class="grid cols-4">
            <div class="field"><label for="pr-mem">RAM (MB)</label><input id="pr-mem" type="number" min="64" value="1024"></div>
            <div class="field"><label for="pr-disk">Disk (MB)</label><input id="pr-disk" type="number" min="128" value="8192"></div>
            <div class="field"><label for="pr-cpu">CPU %</label><input id="pr-cpu" type="number" min="1" max="100" value="100"></div>
            <div class="field"><label for="pr-net">Max network (Mb/s) <small>0 = unlimited</small></label><input id="pr-net" type="number" min="0" value="0" placeholder="0"></div>
          </div>
          <div class="field"><label for="pr-port">Endpoint port <small>${t("runway_port_hint")}</small></label><div class="field-input">${ic("link", 14)}<input id="pr-port" type="number" min="1" max="65535" placeholder="auto"></div></div>
          <label class="check-row"><input type="checkbox" id="pr-start"><span class="check-box">${ic("check", 13, 2.4)}</span><span>${t("runway_start_on_create")}</span></label>
          <div id="pr-errors" class="pr-errors" hidden></div>
        </div>
        <div class="provisioner-pane provisioner-preview">
          <div class="db-pane-head"><b>${ic("terminal", 13)} ${t("runway_preview")}</b></div>
          <div class="provisioner-block"><label>${t("runway_launch_cmd")}</label><div class="code-block pr-command" id="pr-launch"></div></div>
          <div class="provisioner-block"><label>${t("runway_resources")}</label><div class="provisioner-summary" id="pr-summary"></div></div>
          <div class="provisioner-block"><label>${t("runway_endpoint")}</label><div id="pr-ports"></div></div>
          <div class="provisioner-block" id="pr-problems" hidden></div>
        </div>
      </div>
      <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="adminCreateServer">${ic("plus", 14)}<span>${t("create")}</span></button></div>
    </div>`;
    document.body.appendChild(modal);
    /* A11y: return focus to the trigger element when the composer closes —
       every close path (closeModal, Escape, create) removes the modal node. */
    const prevFocus = document.activeElement;
    new MutationObserver((_muts, obs) => {
      if (!modal.isConnected) {
        obs.disconnect();
        if (prevFocus && document.contains(prevFocus)) prevFocus.focus();
      }
    }).observe(document.body, { childList: true, subtree: true });
    const renderVars = () => {
      const b = prBlueprint;
      const varsBox = $("#pr-vars");
      varsBox.innerHTML = (b.variables || []).length ? (b.variables || []).map((v, i) => `<div class="field pr-var">
        <label for="pr-var-${i}">${esc(v.name)} <code>${esc(v.env_var)}</code> ${prKindLabel(v.kind)}${v.required ? '<span class="badge">required</span>' : ""}${v.user_editable ? "" : '<span class="badge">locked</span>'}</label>
        ${prVarInput(v, i)}
        ${v.description ? `<small>${esc(v.description)}</small>` : ""}
      </div>`).join("") : `<p class="muted">${t("runway_no_vars")}</p>`;
    };
    const prRefresh = () => {
      const b = prBlueprint;
      const errors = [];
      const vars = new Map();
      const secrets = new Set((b?.variables || []).filter((v) => !v.user_viewable).map((v) => v.env_var));
      $$("#pr-vars [data-pvar]").forEach((inp) => {
        const env = inp.dataset.pvar;
        const decl = (b?.variables || []).find((x) => x.env_var === env);
        const raw = inp.dataset.bool ? String(inp.checked) : inp.value;
        vars.set(env, raw);
        const err = prValidateVar(decl, raw);
        if (err) { errors.push(err); inp.classList.add("pr-invalid"); }
        else inp.classList.remove("pr-invalid");
      });
      const name = $("#pr-name").value.trim();
      if (!name) errors.push("name is required");
      const mem = Math.max(0, +$("#pr-mem").value || 0);
      const disk = Math.max(0, +$("#pr-disk").value || 0);
      const cpu = Math.max(0, +$("#pr-cpu").value || 0);
      const net = Math.max(0, +$("#pr-net").value || 0);
      // Placement: resolve the chosen target, reveal auto hints, and warn in
      // the preview when a named agent can't fit the requested size.
      const place = $("#pr-place")?.value || "auto";
      const isAuto = place === "auto";
      const autoBox = $("#pr-auto-hints");
      if (autoBox) autoBox.hidden = !isAuto;
      let placeTargetLabel = t("place_auto");
      let placeWarn = "";
      if (isAuto) {
        const tags = ($("#pr-tags")?.value || "").split(",").map((s) => s.trim()).filter(Boolean);
        const loc = ($("#pr-loc")?.value || "").trim().toLowerCase();
        const pool = nodes.filter((n) =>
          (!loc || (n.location || "").toLowerCase() === loc) &&
          tags.every((tag) => (n.tags || []).includes(tag)));
        placeTargetLabel = t("place_auto");
        placeWarn = pool.length ? t("place_auto_pool", { n: pool.length }) : t("place_auto_none");
      } else if (place.startsWith("node:")) {
        const nn = place.slice(5);
        const node = nodes.find((n) => n.name === nn);
        placeTargetLabel = nn;
        if (node && ((+node.available_memory_mb || 0) < mem || (+node.available_disk_mb || 0) < disk)) {
          placeWarn = t("place_no_room", { node: nn });
        }
      }
      const placeHint = $("#pr-place-hint");
      if (placeHint) placeHint.textContent = isAuto ? t("place_hint_auto") : "";
      const portRaw = $("#pr-port").value.trim();
      let port = null;
      if (portRaw) {
        port = +portRaw;
        if (!Number.isInteger(port) || port < 1 || port > 65535) errors.push("port must be 1–65535");
      }
      const startup = b?.startup || "";
      const r = startup ? prRenderLaunch(startup, vars, { name, port: port === null ? "" : String(port), memory_mb: mem, disk_mb: disk, cpu_percent: cpu }, secrets) : null;
      $("#pr-launch").innerHTML = r ? r.html : `<span class="muted">${t("runway_no_launch")}</span>`;
      const renderProblems = (r ? r.problems : []).slice();
      if (placeWarn) renderProblems.push(placeWarn);
      if (renderProblems.length) {
        $("#pr-problems").hidden = false;
        $("#pr-problems").innerHTML = renderProblems.map((p) => `<div class="pr-problem warn">${ic("alert", 12)}${esc(p)}</div>`).join("");
      } else { $("#pr-problems").hidden = true; $("#pr-problems").innerHTML = ""; }
      $("#pr-summary").innerHTML = `
        <div class="metric-line"><span>${t("place_target")}</span><b>${esc(placeTargetLabel)}</b></div>
        <div class="metric-line"><span>${t("ram")}</span><b>${mem} MB</b></div>
        <div class="metric-line"><span>${t("disk")}</span><b>${disk} MB</b></div>
        <div class="metric-line"><span>${t("cpu")}</span><b>${cpu}%</b></div>
        <div class="metric-line"><span>Network</span><b>${net ? net + " Mb/s" : "unlimited"}</b></div>
        <div class="metric-line"><span>Runtime</span><b>${esc(b?.runtime_hint || "native")}</b></div>`;
      $("#pr-ports").innerHTML = port === null
        ? `<span class="allocation-chip muted">${ic("link", 12)}no endpoint yet</span>`
        : `<button class="allocation-chip" data-act="copyText" data-text="${esc(location.hostname + ":" + port)}">${ic("link", 12)}${esc(location.hostname)}:${port}${ic("copy", 11)}</button>`;
      const errBox = $("#pr-errors");
      const createBtn = modal.querySelector('[data-act="adminCreateServer"]');
      if (errors.length) {
        errBox.hidden = false;
        errBox.innerHTML = errors.map((p) => `<div class="pr-problem">${ic("alert", 12)}${esc(p)}</div>`).join("");
        createBtn.disabled = true;
      } else { errBox.hidden = true; errBox.innerHTML = ""; createBtn.disabled = false; }
    };
    $("#pr-blueprint").addEventListener("change", (e) => {
      prBlueprint = blueprints.find((b) => b.id === +e.target.value) || prBlueprint;
      renderVars();
      prRefresh();
    });
    modal.addEventListener("input", (e) => { if (e.target.closest("#pr-vars, #pr-name, #pr-port, #pr-mem, #pr-disk, #pr-cpu, #pr-net, #pr-tags, #pr-loc")) prRefresh(); });
    modal.addEventListener("change", (e) => { if (e.target.closest("#pr-vars, #pr-blueprint, #pr-start, #pr-place")) prRefresh(); });
    renderVars();
    prRefresh();
  } catch (e) { toast(e.message, "error"); }
}
async function adminCreateServer(btn) {
  try {
    const variables = {};
    $$("#pr-vars [data-pvar]").forEach((inp) => { variables[inp.dataset.pvar] = inp.dataset.bool ? String(inp.checked) : inp.value; });
    const portRaw = $("#pr-port").value.trim();
    const body = {
      name: $("#pr-name").value,
      user_id: +$("#pr-user").value,
      blueprint_id: +$("#pr-blueprint").value,
      memory_mb: +$("#pr-mem").value,
      disk_mb: +$("#pr-disk").value,
      cpu_percent: +$("#pr-cpu").value,
      network_mbps: +$("#pr-net").value || 0,
      variables,
      start_on_create: $("#pr-start").checked,
    };
    // Placement -> backend contract: auto (+tags/location) | node name.
    const place = $("#pr-place")?.value || "auto";
    if (place === "auto") {
      body.node = "auto";
      const tags = ($("#pr-tags")?.value || "").split(",").map((s) => s.trim()).filter(Boolean);
      if (tags.length) body.node_tags = tags;
      const loc = ($("#pr-loc")?.value || "").trim();
      if (loc) body.location = loc;
    } else if (place.startsWith("node:")) {
      body.node = place.slice(5);
    }
    if (portRaw) body.port = +portRaw;
    await api("/servers", { method: "POST", body: JSON.stringify(body) });
    const m = btn ? btn.closest(".modal") : null;
    (m || $(".modal"))?.remove();
    toast(t("created"), "success");
    if ($("#admin-body")) adminServers();
    else location.hash = "#/"; // composer opened from the Pulse launch runway — re-route refreshes the fleet
  } catch (e) { toast(e.message, "error"); }
}

async function adminUsers() {
  $("#admin-body").innerHTML = `<div class="card"><div class="card-head"><h3>${t("users")}</h3><button class="btn primary sm" data-act="adminNewUser">${ic("plus", 14)}<span>${t("create_user")}</span></button></div><div id="a-users"></div></div>`;
  try {
    const res = await api("/admin/users");
    const users = res.data || res;
    $("#a-users").innerHTML = `<div class="tbl-wrap"><table class="tbl"><thead><tr><th>ID</th><th>${t("username")}</th><th>Email</th><th>Admin</th><th>Active</th><th>2FA</th><th></th></tr></thead><tbody>` +
      users.map((u) => `<tr>
        <td>${u.id}</td><td><a class="link-strong" href="#/admin/users/${u.id}">${esc(u.username)}</a></td><td>${esc(u.email)}</td>
        <td>${u.root_admin ? '<span class="pill success plain">'+ic("shield", 12)+'admin</span>' : "—"}</td><td>${u.active ? '<span class="pill running">active</span>' : '<span class="pill error">off</span>'}</td>
        <td>${u.twofa_enabled ? ic("shield", 15) : "—"}</td>
        <td><div class="actions"><button class="btn xs ghost" data-act="adminToggleUser" data-id="${u.id}" data-field="root_admin" data-val="${u.root_admin ? "0" : "1"}">${u.root_admin ? "demote" : "promote"}</button>
        <button class="btn xs ghost" data-act="adminToggleUser" data-id="${u.id}" data-field="active" data-val="${u.active ? "0" : "1"}">${u.active ? "disable" : "enable"}</button>
        <button class="icon-btn sm danger" data-act="adminDeleteUser" data-id="${u.id}" data-name="${esc(u.username)}">${ic("trash", 15)}</button></div></td>
      </tr>`).join("") + `</tbody></table></div>`;
  } catch (e) {
    const box = $("#a-users");
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_users")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminUsersRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
async function adminToggleUser(id, field, val) {
  if (field === "root_admin") {
    const label = val ? "promote to administrator" : "demote to member";
    if (!await vpConfirm(`Really ${label} this user? They gain full panel control.`, "Confirm role change")) return;
  }
  try { await api(`/admin/users/${id}`, { method: "PATCH", body: JSON.stringify({ [field]: val }) }); adminUsers(); } catch (e) { toast(e.message, "error"); }
}
async function adminDeleteUser(id, name) {
  if (!await vpDestroy({
    kind: "user", target: name || `user-${id}`,
    consequences: ["Their servers stay but lose their owner", "Active sessions and API keys are revoked", "This cannot be undone"],
  })) return;
  try { await api(`/admin/users/${id}`, { method: "DELETE" }); adminUsers(); } catch (e) { toast(e.message, "error"); }
}
function adminNewUser() {
  const modal = document.createElement("div"); modal.className = "modal";
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("user", 15)} ${t("create_user")}</b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div>
    <div class="field"><label>${t("username")}</label><input id="nu-user"></div>
    <div class="field"><label>Email</label><input id="nu-email"></div>
    <div class="field"><label>${t("password")}</label><input id="nu-pass" type="password"></div>
    <label class="check-row"><input type="checkbox" id="nu-admin"><span class="check-box">${ic("check", 13, 2.4)}</span><span>Administrator</span></label>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="adminCreateUser">${ic("plus", 14)}<span>${t("create")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
}
async function adminCreateUser() {
  try { await api("/admin/users", { method: "POST", body: JSON.stringify({ username: $("#nu-user").value, email: $("#nu-email").value, password: $("#nu-pass").value, root_admin: $("#nu-admin").checked }) }); $(".modal")?.remove(); adminUsers(); toast(t("created"), "success"); }
  catch (e) { toast(e.message, "error"); }
}
/* ============================================================
   Squads — group members into teams and assign them servers.
   Admin API contract: GET/POST /api/admin/squads, PATCH/DELETE
   /api/admin/squads/:id, GET /api/admin/squads/:id,
   POST /api/admin/squads/:id/members {user_id,role},
   PATCH/DELETE /api/admin/squads/:id/members/:uid,
   PUT /api/admin/squads/:id/servers {server_ids:[...]}.
   List shape {id,name,member_count,server_count}; detail adds
   members:[{id,username,role}] and servers:[{id,name}].
   ============================================================ */
const SQ_ROLES = ["viewer", "operator", "developer", "manager"];
const roleLabel = (r) => { const k = "role_" + (r || ""); const v = t(k); return v === k ? String(r || "") : v; };

async function adminSquads() {
  $("#admin-body").innerHTML = `<div class="card"><div class="card-head"><h3>${ic("users", 15)} ${t("squads")}</h3><button class="btn primary sm" data-act="adminNewSquad">${ic("plus", 14)}<span>${t("squad_new")}</span></button></div><div id="a-squads"></div></div>`;
  try {
    const res = await api("/admin/squads");
    const squads = res.data || res || [];
    const box = $("#a-squads");
    box.innerHTML = squads.length
      ? `<div class="tbl-wrap"><table class="tbl"><thead><tr><th>${t("name")}</th><th>${t("squad_members")}</th><th>${t("squad_servers")}</th><th></th></tr></thead><tbody>` +
        squads.map((q) => `<tr>
          <td><a class="link-strong" href="#/admin/squads/${q.id}">${esc(q.name || `squad-${q.id}`)}</a></td>
          <td>${+q.member_count || +q.members_count || 0}</td>
          <td>${+q.server_count || +q.servers_count || 0}</td>
          <td><div class="actions"><button class="btn xs ghost" data-act="adminEditSquad" data-id="${q.id}" data-name="${esc(q.name || "")}">${ic("pencil", 13)}<span>${t("edit")}</span></button><button class="icon-btn sm danger" data-act="adminDeleteSquad" data-id="${q.id}" data-name="${esc(q.name || "")}" title="${t("delete")}" aria-label="${t("delete")}">${ic("trash", 15)}</button></div></td>
        </tr>`).join("") + `</tbody></table></div>`
      : `<div class="empty">${ic("users", 36)}<p>${t("squad_no_squads")}</p></div>`;
  } catch (e) {
    const box = $("#a-squads");
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_squads")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminSquadsRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
function adminNewSquad() {
  const modal = document.createElement("div"); modal.className = "modal";
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("users", 15)} ${t("squad_new")}</b><button class="icon-btn" data-act="closeModal" aria-label="${t("close")}">${ic("x", 16)}</button></div>
    <div class="modal-body"><div class="field"><label>${t("squad_name")}</label><input id="sq-name" maxlength="80" placeholder="${esc(t("squad_name_ph"))}"></div></div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="adminCreateSquad">${ic("plus", 14)}<span>${t("create")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
}
async function adminCreateSquad() {
  const name = $("#sq-name")?.value.trim(); if (!name) { toast(t("e_required_name"), "warn"); return; }
  try { await api("/admin/squads", { method: "POST", body: JSON.stringify({ name }) }); $(".modal")?.remove(); adminSquads(); toast(t("squad_created"), "success"); }
  catch (e) { toast(e.message, "error"); }
}
function adminEditSquad(el) {
  const modal = document.createElement("div"); modal.className = "modal";
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("pencil", 15)} ${t("edit")} — ${esc(el.dataset.name || `squad-${el.dataset.id}`)}</b><button class="icon-btn" data-act="closeModal" aria-label="${t("close")}">${ic("x", 16)}</button></div>
    <div class="modal-body"><div class="field"><label>${t("squad_name")}</label><input id="sq-name" maxlength="80" value="${esc(el.dataset.name || "")}"></div></div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="adminSaveSquad" data-id="${el.dataset.id}">${ic("save", 14)}<span>${t("save")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
}
async function adminSaveSquad(el) {
  const id = +el.dataset.id, name = $("#sq-name")?.value.trim(); if (!name) { toast(t("e_required_name"), "warn"); return; }
  try {
    await api(`/admin/squads/${id}`, { method: "PATCH", body: JSON.stringify({ name }) });
    $(".modal")?.remove(); toast(t("squad_saved"), "success");
    if (location.hash.includes(`/admin/squads/${id}`)) adminSquadDetail(id); else adminSquads();
  } catch (e) { toast(e.message, "error"); }
}
async function adminDeleteSquad(el) {
  const id = +el.dataset.id, name = el.dataset.name || `squad-${id}`;
  if (!await vpConfirm(t("squad_delete_confirm").replace("{name}", name), t("squads"))) return;
  try {
    await api(`/admin/squads/${id}`, { method: "DELETE" });
    toast(t("squad_deleted"), "success");
    if (location.hash.includes(`/admin/squads/${id}`)) location.hash = "#/admin/squads"; else adminSquads();
  } catch (e) { toast(e.message, "error"); }
}
async function adminSquadDetail(id) {
  $("#admin-body").innerHTML = `<div class="empty">${ic("users", 40)}<p>${t("loading")}</p></div>`;
  try {
    /* /admin/users (the add-member picker source) is AdminUser-scoped, so it
       is fetched for root only; the detail itself is served to any
       authenticated user via squad_get. */
    const qs = [api(`/admin/squads/${id}`), api("/servers")];
    if (state.user?.root_admin) qs.push(api("/admin/users"));
    const [sqRes, serversRes, usersRes] = await Promise.all(qs);
    renderSquadDetail(sqRes.data || sqRes, usersRes?.data || usersRes || [], serversRes.data || serversRes || []);
  } catch (e) {
    const box = $("#admin-body");
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_squad")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminSquadRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
function renderSquadDetail(sq, users, servers) {
  const id = +sq.id;
  const isRoot = !!state.user?.root_admin;
  /* my_role: caller's membership role from squad_get — "manager" for the
     squad creator and for root admins even without a membership row, the
     member's real role otherwise, null for non-members.
     Client capability hints mirroring backend enforcement (api/servers.rs):
       canManage — member add/remove, role select, server assignment, rename:
         root OR my_role === "manager" (rename is manager-or-root server-side).
       canDelete — shown for manager-or-root; the DELETE endpoint itself is
         creator-or-root, so a non-creator manager sees the button but gets a
         403 toast. my_role cannot tell a creator apart from a plain manager
         (both report "manager"), so this is the tightest client-side hint.
     The add-member PICKER renders for root only this iteration: /admin/users
     is AdminUser-scoped and no user-listing endpoint exists for managers. The
     backend accepts member-add from any manager, so this is a documented UI
     gap, not an API restriction. */
  const myRole = typeof sq.my_role === "string" ? sq.my_role : null;
  const canManage = isRoot || myRole === "manager";
  const canDelete = canManage;
  const members = Array.isArray(sq.members) ? sq.members : [];
  const raw = Array.isArray(sq.servers) ? sq.servers : [];
  const assigned = new Set(raw.map((s) => (s && typeof s === "object" ? +s.id : +s)));
  (Array.isArray(sq.server_ids) ? sq.server_ids : []).forEach((i) => assigned.add(+i));
  const meId = +state.user?.id;
  const memberRow = (m) => canManage
    ? `<select data-act="sqMemberRole" data-act-change data-sid="${id}" data-mid="${m.id}" aria-label="${esc(t("squad_role"))}">${SQ_ROLES.map((r) => `<option value="${r}"${r === m.role ? " selected" : ""}>${esc(roleLabel(r))}</option>`).join("")}</select>
       <button class="icon-btn sm danger" data-act="sqMemberDel" data-sid="${id}" data-mid="${m.id}" data-name="${esc(m.username || "")}" title="${t("delete")}" aria-label="${t("delete")}">${ic("x", 15)}</button>`
    : (+m.id === meId
        ? `<span class="pill success plain">${esc(t("squad_my_role").replace("{role}", roleLabel(m.role)))}</span>`
        : `<span class="pill info plain">${esc(roleLabel(m.role))}</span>`);
  const memberRows = members.length ? members.map((m) => `<div class="member-row">
    <span class="avatar mini">${esc(String(m.username || "?")[0]).toUpperCase()}</span>
    <b>${esc(m.username || `user-${m.id}`)}</b>
    <span class="spacer"></span>
    ${memberRow(m)}
  </div>`).join("") : `<div class="muted">${t("squad_no_members")}</div>`;
  const userOpts = users.map((u) => `<option value="${u.id}">${esc(u.username)}</option>`).join("");
  const serverGrid = servers.length
    ? canManage
      ? `<div class="squad-server-grid">${servers.map((s) => `<label class="check-row"><input type="checkbox" class="sq-server" value="${s.id}"${assigned.has(+s.id) ? " checked" : ""}><span class="check-box">${ic("check", 13, 2.4)}</span><span>${esc(s.name || `server-${s.id}`)}</span></label>`).join("")}</div>
         <button class="btn primary sm" data-act="sqServersSave" data-sid="${id}">${ic("save", 14)}<span>${t("squad_save_servers")}</span></button>`
      : `<div class="membership-chips">${servers.filter((s) => assigned.has(+s.id)).map((s) => `<span class="membership-chip">${esc(s.name || `server-${s.id}`)}</span>`).join("")}</div>`
    : `<div class="muted">${t("no_servers")}</div>`;
  $("#admin-body").innerHTML = `
    <div class="squad-detail-head">
      <a href="${isRoot ? "#/admin/squads" : "#/"}" class="btn sm ghost">${ic("chevron_left", 13)}<span>${t("back")}</span></a>
      <h2>${ic("users", 18)} ${esc(sq.name || `squad-${id}`)}</h2>
      <span class="pill info plain">${members.length} ${t("squad_members")}</span>
      <span class="pill plain">${assigned.size} ${t("squad_servers")}</span>
      ${canDelete ? `<div class="actions">
        <button class="btn sm ghost" data-act="adminEditSquad" data-id="${id}" data-name="${esc(sq.name || "")}">${ic("pencil", 13)}<span>${t("edit")}</span></button>
        <button class="btn sm danger" data-act="adminDeleteSquad" data-id="${id}" data-name="${esc(sq.name || "")}">${ic("trash", 13)}<span>${t("delete")}</span></button>
      </div>` : ""}
    </div>
    <div class="card">
      <div class="card-head"><h3>${ic("users", 15)} ${t("squad_members")} <span class="badge">${members.length}</span></h3></div>
      <div id="sq-members">${memberRows}</div>
      ${canManage && isRoot ? `<div class="member-row add">
        <select id="sq-user" aria-label="${esc(t("squad_add_member"))}"><option value="">${esc(t("squad_user_ph"))}</option>${userOpts}</select>
        <select id="sq-role" aria-label="${esc(t("squad_role"))}">${SQ_ROLES.map((r) => `<option value="${r}">${esc(roleLabel(r))}</option>`).join("")}</select>
        <button class="btn primary sm" data-act="sqMemberAdd" data-sid="${id}">${ic("plus", 13)}<span>${t("squad_add_member")}</span></button>
      </div>` : ""}
    </div>
    <div class="card">
      <div class="card-head"><h3>${ic("server", 15)} ${t("squad_server_assignment")} <span class="badge">${assigned.size}</span></h3></div>
      ${serverGrid}
    </div>`;
}
async function sqMemberAdd(el) {
  const id = +el.dataset.sid;
  const sel = $("#sq-user"); const user_id = sel ? +sel.value : 0;
  const role = $("#sq-role")?.value || "viewer";
  if (!user_id) { toast(t("e_pick_user"), "warn"); return; }
  try { await api(`/admin/squads/${id}/members`, { method: "POST", body: JSON.stringify({ user_id, role }) }); toast(t("member_added"), "success"); adminSquadDetail(id); }
  catch (e) { toast(e.message, "error"); }
}
async function sqMemberRole(el) {
  try { await api(`/admin/squads/${el.dataset.sid}/members/${el.dataset.mid}`, { method: "PATCH", body: JSON.stringify({ role: el.value }) }); toast(t("member_role_updated"), "success"); }
  catch (e) { toast(e.message, "error"); }
}
async function sqMemberDel(el) {
  const id = +el.dataset.sid, uid = +el.dataset.mid, name = el.dataset.name || `user-${uid}`;
  if (!await vpConfirm(t("member_remove_confirm").replace("{name}", name))) return;
  try { await api(`/admin/squads/${id}/members/${uid}`, { method: "DELETE" }); toast(t("member_removed"), "success"); adminSquadDetail(id); }
  catch (e) { toast(e.message, "error"); }
}
async function sqServersSave(el) {
  const id = +el.dataset.sid;
  const server_ids = $$(".sq-server:checked").map((c) => +c.value);
  try { await api(`/admin/squads/${id}/servers`, { method: "PUT", body: JSON.stringify({ server_ids }) }); toast(t("squad_servers_saved"), "success"); adminSquadDetail(id); }
  catch (e) { toast(e.message, "error"); }
}
/* User detail (admin Team view): read-only squad memberships. Backed by
   GET /api/admin/users/:id which returns squads:[{id,name,role}]. */
async function adminUserDetail(id) {
  $("#admin-body").innerHTML = `<div class="empty">${ic("user", 40)}<p>${t("loading")}</p></div>`;
  try {
    const res = await api(`/admin/users/${id}`);
    const u = res.data || res;
    const memberships = (u.squads || []).map((q) => ({ squad: q.name || `squad-${q.id}`, role: q.role }));
    $("#admin-body").innerHTML = `
      <div class="squad-detail-head">
        <a href="#/admin/users" class="btn sm ghost">${ic("chevron_left", 13)}<span>${t("back")}</span></a>
        <h2>${ic("user", 18)} ${esc(u.username)}</h2>
        <span class="pill ${u.active ? "running" : "error"}"><i></i>${u.active ? "active" : "off"}</span>
        ${u.root_admin ? `<span class="pill success plain">${ic("shield", 12)} admin</span>` : ""}
      </div>
      <div class="card">
        <div class="card-head"><h3>${ic("user", 15)} ${t("user_detail")}</h3></div>
        <div class="user-detail-grid">
          <div class="field"><label>ID</label><code>${u.id}</code></div>
          <div class="field"><label>${t("username")}</label><b>${esc(u.username)}</b></div>
          <div class="field"><label>Email</label><span>${esc(u.email || "—")}</span></div>
        </div>
      </div>
      <div class="card">
        <div class="card-head"><h3>${ic("users", 15)} ${t("squad_memberships")} <span class="badge">${memberships.length}</span></h3></div>
        ${memberships.length ? `<div class="membership-chips">${memberships.map((m) => `<span class="membership-chip">${esc(m.squad)}<span class="pill info plain">${esc(roleLabel(m.role))}</span></span>`).join("")}</div>` : `<div class="muted">${t("squad_no_memberships")}</div>`}
      </div>
      ${u.twofa_enabled && +u.id !== +state.user?.id ? `
      <div class="card">
        <div class="card-head"><h3>${ic("shield", 15)} ${t("twofa")}</h3></div>
        <p class="muted mb-10">2FA is enabled for this user. Resetting clears their secret and recovery codes and forces re-enrollment.</p>
        <button class="btn danger-ghost" data-act="adminReset2fa" data-id="${u.id}" data-name="${esc(u.username)}">${ic("shield", 14)}<span>${t("reset_2fa")}</span></button>
      </div>` : ""}`;
  } catch (e) {
    const box = $("#admin-body");
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_user_detail")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminUserDetailRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}

async function adminReset2fa(id, name) {
  if (!await vpConfirm(t("reset_2fa_confirm").replace("{name}", name || `user-${id}`))) return;
  try {
    await api(`/admin/users/${id}/2fa/reset`, { method: "POST" });
    toast(t("reset_2fa") + " ✓", "success");
    adminUserDetail(id);
  } catch (e) { toast(e.message, "error"); }
}

async function adminBlueprints() {
  const inReg = location.hash.includes("/registry");
  $("#admin-body").innerHTML = `<section class="nodes-header"><div><span class="eyebrow">${t("bp_eyebrow")}</span><h2>${t("bp_title")}</h2><p>${t("bp_sub")}</p></div>${inReg ? "" : `<button class="btn primary sm" data-act="adminNewBlueprint">${ic("plus",14)}<span>${t("new_blueprint")}</span></button>`}</section><div class="tabs bp-subtabs"><a href="#/admin/blueprints" class="${inReg ? "" : "active"}">${ic("box",14)}<span>${t("reg_library")}</span></a><a href="#/admin/blueprints/registry" class="${inReg ? "active" : ""}">${ic("globe",14)}<span>${t("registry")}</span></a></div><div id="a-blueprints" class="blueprint-grid"></div>`;
  if (inReg) return adminRegistry();
  try {
    const [res, regRes] = await Promise.all([api("/blueprints"), api("/blueprints/registry").catch((e) => { console.warn("blueprint registry unavailable", e); return null; })]);
    const blueprints = res.data || res;
    const published = new Set((regRes?.data?.packages || []).map((p) => p.source_uuid).filter(Boolean));
    $("#a-blueprints").innerHTML = blueprints.length ? blueprints.map((definition) => {const count=definition.variables?.length||0;const kind=String(definition.category||"").toLowerCase();const symbol=kind==="database"?"database":kind==="web"?"globe":kind==="game"?"zap":kind==="generic"?"terminal":"blueprint";return `<article class="blueprint-card"><div class="blueprint-card-head"><span class="blueprint-symbol">${ic(symbol,20)}</span><div><h3>${esc(definition.name)}</h3><span>${esc(definition.category)} · VoltSpec</span></div></div><p>${esc(definition.description || "Reusable isolated workload plan")}</p><div class="blueprint-command"><span>Launch</span><code title="${esc(definition.startup || "operator-defined")}">${esc(definition.startup || "operator-defined")}</code></div><div class="blueprint-card-foot"><span>${count} ${count===1?'input':'inputs'}</span><div class="actions"><button class="icon-btn sm" title="Versions & drift" data-act="bpInspect" data-id="${definition.id}">${ic("clock",15)}</button><button class="icon-btn sm" title="Export VoltSpec" data-act="adminBlueprintExport" data-id="${definition.id}">${ic("download",15)}</button><button class="icon-btn sm danger" title="Delete blueprint" data-act="adminDeleteBlueprint" data-id="${definition.id}" data-name="${esc(definition.name)}">${ic("trash",15)}</button></div></div></article>`}).join("") : `<div class="context-empty">${ic("blueprint",28)}<div><b>No blueprints yet</b><span>Create the first portable VoltSpec launch plan.</span></div></div>`;
    if (blueprints.length) blueprints.forEach((def, i) => {
      const card = $$("#a-blueprints .blueprint-card")[i];
      if (!card) return;
      const foot = card.querySelector(".blueprint-card-foot");
      const countSpan = foot?.querySelector("span");
      if (published.has(def.uuid) && countSpan) {
        const pill = document.createElement("span");
        pill.className = "pill plain";
        pill.innerHTML = ic("check", 10) + " " + esc(t("reg_published"));
        countSpan.after(pill);
      }
      const actions = foot?.querySelector(".actions");
      if (actions) {
        const pb = document.createElement("button");
        pb.className = "icon-btn sm";
        pb.title = "Publish to registry";
        pb.setAttribute("aria-label", "Publish to registry");
        pb.dataset.act = "regPublish";
        pb.dataset.id = String(def.id);
        pb.dataset.name = def.name;
        pb.innerHTML = ic("upload", 15);
        actions.prepend(pb);
      }
    });
  } catch (e) {
    const box = $("#a-blueprints");
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_blueprints")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminBlueprintsRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
async function adminBlueprintExport(id){try{const r=await api(`/blueprints/${id}/export`);await navigator.clipboard?.writeText(r.json);toast(t("bp_copied"),"success")}catch(e){toast(e.message,"error")}}
async function adminDeleteBlueprint(id, name) {
  if (!await vpDestroy({
    kind: "blueprint", target: name || `blueprint-${id}`,
    consequences: ["Servers built from it keep running", "Rebuild and drift checks stop working", "This cannot be undone"],
  })) return;
  try { await api(`/blueprints/${id}`, { method: "DELETE" }); adminBlueprints(); } catch (e) { toast(e.message, "error"); }
}
function adminNewBlueprint(){
  const modal=document.createElement("div");
  modal.className="modal";
  modal.innerHTML=`<div class="modal-card blueprint-create-modal">
    <div class="modal-head"><div><b>${t("new_blueprint")}</b><span class="modal-subtitle">${t("bp_new_sub")}</span></div><button class="icon-btn" aria-label="${t("close")}" data-act="closeModal">${ic("x",16)}</button></div>
    <div class="modal-body modal-form-grid">
      <div class="field"><label for="nb-name">${t("name")}</label><input type="text" id="nb-name" placeholder="Velocity Proxy" autocomplete="off"><small>${t("bp_name_hint")}</small></div>
      <div class="field"><label for="nb-category">${t("bp_category")}</label><input type="text" id="nb-category" value="application" placeholder="application" autocomplete="off"><small>${t("bp_category_hint")}</small></div>
      <div class="field field-wide"><label for="nb-runtime">${t("bp_runtime")}</label><input type="text" id="nb-runtime" value="native" placeholder="native" autocomplete="off"></div>
      <div class="field field-wide"><label for="nb-launch">${t("bp_launch")}</label><input type="text" id="nb-launch" placeholder="java -jar app.jar" autocomplete="off"><small>${t("bp_launch_hint")}</small></div>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="adminCreateBlueprint">${ic("plus",14)}<span>${t("bp_create")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
}
async function adminCreateBlueprint(btn){try{await api("/blueprints",{method:"POST",body:JSON.stringify({name:$("#nb-name").value,category:$("#nb-category").value,runtime_hint:$("#nb-runtime").value,startup:$("#nb-launch").value})});const m=btn?btn.closest(".modal"):null;(m||$(".modal"))?.remove();adminBlueprints();toast(t("t_bp_created"), "success")}catch(e){toast(e.message,"error")}}
/* ---------- blueprint studio: versions, drift & pinning ---------- */
let bpModal = { id: 0, current: 1, revs: [] };
async function bpInspect(id) {
  bpModal = { id, current: 1, revs: [] };
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.innerHTML = `<div class="modal-card big bp-modal">
    <div class="modal-head"><b>${ic("clock", 16)} <span id="bp-title">Blueprint #${id}</span> <span class="badge">versions & drift</span></b><button class="icon-btn" data-act="closeModal">${ic("x", 16)}</button></div>
    <div class="bp-modal-body">
      <div class="bp-pane">
        <div class="bp-pane-head"><b>Revisions</b><button class="icon-btn sm" title="refresh" data-act="bpLoadRevisions" data-id="${id}">${ic("refresh_ccw", 14)}</button></div>
        <div id="bp-rev-list" class="bp-list"><div class="empty">${ic("clock", 32)}<p>${t("loading")}</p></div></div>
      </div>
      <div class="bp-pane bp-pane-snap">
        <div class="bp-pane-head"><b>Snapshot</b><span id="bp-snap-meta" class="muted"></span></div>
        <pre id="bp-snap" class="bp-snap-pre">Select a revision to inspect its snapshot JSON.</pre>
      </div>
      <div class="bp-pane">
        <div class="bp-pane-head"><b>Drift</b><button class="icon-btn sm" title="refresh" data-act="bpLoadDrift" data-id="${id}">${ic("refresh_ccw", 14)}</button></div>
        <div id="bp-drift-list" class="bp-list"><div class="empty">${ic("link", 32)}<p>${t("loading")}</p></div></div>
      </div>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button></div>
  </div>`;
  document.body.appendChild(modal);
  const meta = api(`/blueprints/${id}`).catch((e) => { console.warn("blueprint metadata unavailable", e); return null; });
  await bpLoadRevisions(id);
  await bpLoadDrift(id);
  const bp = await meta;
  if (bp?.name) { const h = $("#bp-title"); if (h) h.textContent = bp.name; }
}
async function bpLoadRevisions(id) {
  const box = $("#bp-rev-list");
  if (!box) return;
  try {
    const res = await api(`/blueprints/${id}/revisions`);
    const revs = res.data || [];
    bpModal.revs = revs;
    bpModal.current = revs[0]?.version ?? bpModal.current;
    if (!revs.length) { box.innerHTML = `<div class="context-empty">${ic("clock", 22)}<div><b>No revisions yet</b><span>Every update snapshots the previous state; the first revision appears after an edit.</span></div></div>`; return; }
    box.innerHTML = revs.map((r) => `<div class="bp-rev${r.version === bpModal.current ? " current" : ""}" data-act="bpRevDetail" data-id="${id}" data-version="${r.version}">
      <div class="bp-rev-top"><b>v${r.version}</b>${r.version === bpModal.current ? `<span class="pill running plain">current</span>` : ""}<span class="bp-rev-time">${fmtDate(r.created_at)}</span></div>
      <div class="bp-rev-meta"><span>${esc(r.author || "—")}</span><code title="${esc(r.digest)}">${esc(r.digest.slice(0, 10))}…</code></div>
      ${r.note ? `<div class="bp-rev-note">${esc(r.note)}</div>` : ""}
      <div class="bp-rev-actions"><button class="btn xs danger" data-act="bpRollback" data-id="${id}" data-version="${r.version}">${ic("refresh", 12)}<span>Roll back</span></button></div>
    </div>`).join("");
    bpRevDetail(id, revs[0].version, box.querySelector(".bp-rev"));
  } catch (e) { box.innerHTML = `<div class="context-empty">${ic("alert", 22)}<div><b>${t("err_load_revisions")}</b><span>${esc(e.message)}</span></div></div>`; }
}
async function bpRevDetail(id, version, el) {
  el?.classList.add("active");
  $$(".bp-rev.active").forEach((b) => { if (b !== el) b.classList.remove("active"); });
  const pre = $("#bp-snap");
  if (!pre) return;
  pre.textContent = "Loading v" + version + "…";
  try {
    const res = await api(`/blueprints/${id}/revisions/${version}`);
    const json = JSON.stringify(res.data, null, 2);
    pre.textContent = json;
    const meta = $("#bp-snap-meta");
    if (meta) meta.textContent = `v${version} · ${json.length} bytes`;
  } catch (e) { pre.textContent = "Error: " + e.message; }
}
async function bpRollback(id, version) {
  if (!await vpConfirm(`Roll blueprint #${id} back to revision v${version}? The current state is snapshotted first, so this rollback is itself undoable.`, "Roll back blueprint")) return;
  try {
    const res = await api(`/blueprints/${id}/rollback`, { method: "POST", body: JSON.stringify({ version }) });
    const now = res.data?.version ?? version;
    bpModal.current = now;
    toast(`Rolled back to v${version} — blueprint now at v${now}`, "success");
    await Promise.all([bpLoadRevisions(id), bpLoadDrift(id)]);
  } catch (e) { toast(e.message, "error"); }
}
async function bpLoadDrift(id) {
  const box = $("#bp-drift-list");
  if (!box) return;
  try {
    const res = await api(`/blueprints/${id}/drift`);
    const rows = res.data || [];
    if (!rows.length) { box.innerHTML = `<div class="context-empty">${ic("check", 22)}<div><b>No drift detected</b><span>Every pinned server is on the current revision, or no server uses this blueprint.</span></div></div>`; return; }
    const opts = bpModal.revs.length ? bpModal.revs.map((r) => `<option value="${r.version}"${r.version === bpModal.current ? " selected" : ""}>v${r.version}${r.version === bpModal.current ? " · current" : ""}</option>`).join("") : "";
    box.innerHTML = rows.map((d) => {
      const unpinned = d.pinned_version === 0;
      return `<div class="bp-drift-row">
        <div class="bp-drift-name"><b>${esc(d.server_name)}</b><span class="muted">#${esc(d.server_id)}</span></div>
        <div class="bp-drift-state">${unpinned ? `<span class="pill plain">unpinned</span>` : `<span class="pill warn"><i></i>v${esc(d.pinned_version)} → v${esc(d.current_version)}</span>`}</div>
        <div class="bp-drift-fields">${unpinned ? `<span class="muted">not diffed against a pinned revision</span>` : d.fields.map((f) => `<code>${esc(f)}</code>`).join("") || `<span class="muted">in sync</span>`}</div>
        <div class="bp-drift-actions">
          <select class="bp-pin-select" data-server="${esc(d.server_id)}">${opts || `<option value="${esc(d.current_version)}">v${esc(d.current_version)}</option>`}</select>
          <button class="btn xs ghost" data-act="bpPin" data-bpid="${id}">${ic("link", 12)}<span>Pin</span></button>
          ${unpinned ? "" : `<button class="icon-btn sm danger" title="Unpin server" aria-label="Unpin server" data-act="bpUnpin" data-server="${esc(d.server_id)}" data-bpid="${id}">${ic("x", 13)}</button>`}
        </div>
      </div>`;
    }).join("");
  } catch (e) { box.innerHTML = `<div class="context-empty">${ic("alert", 22)}<div><b>${t("err_load_drift")}</b><span>${esc(e.message)}</span></div></div>`; }
}
async function bpPin(btn, bpId) {
  const sel = btn.closest(".bp-drift-actions").querySelector(".bp-pin-select");
  const serverId = +sel.dataset.server, version = +sel.value;
  if (!await vpConfirm(`Pin server #${serverId} to revision v${version}? Its blueprint_version is set to ${version}; future edits will report drift against this revision.`, "Pin server")) return;
  try { await api(`/servers/${serverId}/blueprint/pin`, { method: "POST", body: JSON.stringify({ version }) }); toast(`Server #${serverId} pinned to v${version}`, "success"); bpLoadDrift(bpId); }
  catch (e) { toast(e.message, "error"); }
}
async function bpUnpin(serverId, bpId) {
  if (!await vpConfirm(`Unpin server #${serverId}? It will no longer be diffed against any revision.`, "Unpin server")) return;
  try { await api(`/servers/${serverId}/blueprint/pin`, { method: "POST", body: JSON.stringify({ version: 0 }) }); toast(`Server #${serverId} unpinned`, "success"); bpLoadDrift(bpId); }
  catch (e) { toast(e.message, "error"); }
}

/* ---------- blueprint studio: VoltSpec registry ---------- */
let regVerState = {};
async function adminRegistry() {
  const box = $("#a-blueprints");
  if (!box) return;
  box.className = "reg-wrap";
  box.innerHTML = `<div class="empty">${ic("globe", 36)}<p>${t("loading")}</p></div>`;
  try {
    const res = await api("/blueprints/registry");
    const data = res.data || {};
    const packages = data.packages || [];
    const local = data.local || {};
    const signing = data.signing || {};
    const groups = {};
    for (const p of packages) (groups[p.id] = groups[p.id] || []).push(p);
    const installed = new Set();
    for (const uuid in local) { const [id, v] = local[uuid]; installed.add(id + "@" + v); }
    const rows = Object.keys(groups).sort().map((id) => {
      const vers = groups[id];
      const p = vers[0];
      const sigBadge = !p.signed ? `<span class="pill plain">${t("reg_unsigned")}</span>`
        : p.signature_valid ? `<span class="pill running plain">${ic("check",10)} ${t("reg_signed")}</span>`
        : `<span class="pill warn">${t("reg_bad_sig")}</span>`;
      const installedHere = installed.has(id + "@" + p.version);
      const want = regVerState[id] ? String(regVerState[id]) : String(p.version);
      const verOpts = vers.map((x) => `<option value="${esc(x.version)}"${String(x.version) === want ? " selected" : ""}>v${esc(x.version)}${installed.has(id + "@" + x.version) ? " ✓" : ""}</option>`).join("");
      return `<div class="reg-row" data-regname="${esc((p.name || id).toLowerCase())}">
        <div class="reg-main">
          <div class="reg-head"><span class="blueprint-symbol">${ic("box", 18)}</span><div><h3>${esc(p.name || id)}</h3><span class="reg-id">${esc(id)}</span></div>${sigBadge}</div>
          <div class="reg-meta"><span>${ic("user",12)} ${esc(p.publisher || "—")}</span><span>${ic("clock",12)} ${esc(p.published_at || "—")}</span><span>${ic("box",12)} ${vers.length} ${t("reg_versions")}</span>${installedHere ? `<span class="pill running plain">${ic("check",10)} ${t("reg_installed")}</span>` : ""}</div>
        </div>
        <div class="reg-actions">
          <select class="reg-ver" data-id="${esc(id)}" aria-label="${esc(t("reg_ver_label").replace("{name}", p.name || id))}">${verOpts}</select>
          <button class="btn primary sm" data-act="regInstall" data-id="${esc(id)}" data-name="${esc(p.name || id)}">${ic("download",14)}<span>${t("reg_install")}</span></button>
        </div>
      </div>`;
    }).join("");
    const fp = signing.fingerprint || "";
    box.innerHTML = `
      <div class="reg-toolbar">
        <div class="f-search">${ic("search",14)}<input id="reg-search" placeholder="${esc(t("reg_search_ph"))}" aria-label="${esc(t("reg_search_ph"))}" autocomplete="off"></div>
        <div class="reg-sign">
          ${signing.enabled
            ? `<span class="muted">${ic("lock",12)} ${t("reg_signing_key")} <code>${esc(fp.slice(0, 8))}…</code></span><button class="btn xs ghost" data-act="regClearKey">${ic("x",12)}<span>${t("reg_clear_key")}</span></button>`
            : `<span class="muted">${ic("lock",12)} ${t("reg_signing_off")}</span><button class="btn xs ghost" data-act="regGenKey">${ic("key",12)}<span>${t("reg_gen_key")}</span></button>`}
        </div>
      </div>
      <div id="reg-list">${rows || `<div class="context-empty">${ic("globe",26)}<div><b>${t("reg_no_packages")}</b><span>${t("reg_empty_hint")}</span></div></div>`}</div>`;
    const search = $("#reg-search");
    if (search) search.addEventListener("input", () => {
      const q = search.value.toLowerCase().trim();
      $$(".reg-row").forEach((row) => { row.style.display = !q || row.dataset.regname.includes(q) ? "" : "none"; });
    });
    $$(".reg-ver").forEach((sel) => sel.addEventListener("change", () => { regVerState[sel.dataset.id] = sel.value; }));
  } catch (e) {
    box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_blueprints")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminBlueprintsRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}
async function regInstall(el) {
  const row = el.closest(".reg-actions");
  const id = el.dataset.id, version = +row.querySelector(".reg-ver").value;
  const name = el.dataset.name || id;
  if (!await vpConfirm(t("reg_install_confirm").replace("{name}", name).replace("{version}", String(version)), t("reg_install"))) return;
  try {
    const r = await api("/blueprints/registry/import", { method: "POST", body: JSON.stringify({ id, version }) });
    toast(r.warning ? t("reg_unsigned_warn") : t("reg_imported"), r.warning ? "warn" : "success");
    adminRegistry();
  } catch (e) { toast(e.message, "error"); }
}
async function regPublish(el) {
  const id = +el.dataset.id, name = el.dataset.name || `blueprint-${id}`;
  if (!await vpConfirm(t("reg_publish_confirm").replace("{name}", name), t("reg_publish"))) return;
  try {
    const r = await api("/blueprints/registry/publish", { method: "POST", body: JSON.stringify({ id }) });
    toast(r.warning ? t("reg_publish_unsigned_warn") : t("reg_published_ok"), r.warning ? "warn" : "success");
  } catch (e) { toast(e.message, "error"); }
}
async function regGenKey() {
  try {
    const r = await api("/settings/registry/signing-key", { method: "POST", body: JSON.stringify({ key: null }) });
    toast(t("reg_key_generated").replace("{fp}", r.fingerprint || ""), "success");
    adminRegistry();
  } catch (e) { toast(e.message, "error"); }
}
async function regClearKey() {
  try {
    await api("/settings/registry/signing-key", { method: "POST", body: JSON.stringify({ key: "" }) });
    toast(t("reg_key_cleared"), "success");
    adminRegistry();
  } catch (e) { toast(e.message, "error"); }
}
async function adminNodes() {
  $("#admin-body").innerHTML = `<section class="nodes-header"><div><span class="eyebrow">${t("fabric_eyebrow")}</span><h2>${t("fabric_title")}</h2><p>${t("fabric_sub")}</p></div><button class="btn primary" data-act="adminNewNode">${ic("plus",14)}<span>${t("qa_attach")}</span></button></section><div id="a-nodes" class="node-grid"></div>`;
  try {
    const res = await api("/nodes"); const values = res.data || [];
    $("#a-nodes").innerHTML = values.length ? values.map(n => { const cpu=Math.min(100,Math.round(n.capacity?.cpu_percent||0)), mem=n.capacity?.memory_total?Math.min(100,Math.round(n.capacity.memory_used/n.capacity.memory_total*100)):0; return `<article class="node-card ${n.online?'online':'offline'}">
      <div class="node-card-head"><div class="node-mark">${ic('server',20)}</div><div><h3>${esc(n.name)}</h3><span>${esc(n.location||'unassigned')}</span></div><span class="pill ${n.online?'running':'offline'}"><i></i>${n.online?'online':'offline'}</span></div>
      <div class="node-endpoint">${ic('link',13)}<code>${esc(n.public_url)}</code></div>
      <div class="node-metrics"><div><span>CPU</span><b>${cpu}%</b><div class="progress"><div data-w="${cpu}"></div></div></div><div><span>Memory</span><b>${mem}%</b><div class="progress"><div class="purple" data-w="${mem}"></div></div></div></div>
      <div class="node-security">${ic('shield',15)}<span>${t("isolation_ns")}</span><b>${n.online?'verified':'pending'}</b></div>
      <div class="node-card-foot"><span>${n.capacity?.servers_running||0} running / ${n.capacity?.servers_total||0} total</span><div class="actions"><button class="icon-btn" title="${t("a_test_node")}" aria-label="${t("a_test_node")}" data-act="nodeTest" data-id="${n.id}">${ic('activity',15)}</button><button class="icon-btn" title="${t("a_reenroll")}" aria-label="${t("a_reenroll")}" data-act="nodeReenroll" data-id="${n.id}">${ic('key',15)}</button><button class="icon-btn danger" title="${t("a_delete_node")}" aria-label="${t("a_delete_node")}" data-act="nodeDelete" data-id="${n.id}" data-name="${esc(n.name)}">${ic('trash',15)}</button></div></div></article>`; }).join('') : emptyState("globe", t("es_nodes_t"), t("es_nodes_h"), { act: "adminNewNode", actLabel: t("qa_attach"), actIcon: "plus" });
    $$("#a-nodes .progress > div[data-w]").forEach((el) => { el.style.width = el.dataset.w + "%"; });
  } catch(e) {
    const box = $("#a-nodes");
    if (box) box.innerHTML = `<div class="node-grid"><div class="card"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_agents")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminNodesRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div></div>`;
  }
}

function adminNewNode() {
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.innerHTML = `<div class="modal-card node-create-modal">
    <div class="modal-head"><div><b>${ic("globe", 15)} ${t("node_new_title")}</b><span class="modal-subtitle">${t("node_new_sub")}</span></div><button class="icon-btn" data-act="closeModal" aria-label="${t("close")}">${ic("x", 16)}</button></div>
    <div class="modal-body modal-form-grid">
      <div class="field"><label for="nn-name">${t("name")}</label><input id="nn-name" placeholder="${t("node_name_ph")}" autocomplete="off"></div>
      <div class="field"><label for="nn-url">${t("node_url")}</label><div class="field-input">${ic("link", 14)}<input id="nn-url" value="http://127.0.0.1:8081" spellcheck="false"></div><small>${t("node_url_hint")}</small></div>
      <div class="field"><label for="nn-location">${t("node_location")}</label><input id="nn-location" placeholder="${t("node_location_ph")}" autocomplete="off"><small>${t("node_location_hint")}</small></div>
      <div class="field"><label for="nn-tags">${t("node_tags")}</label><input id="nn-tags" placeholder="${t("node_tags_ph")}" autocomplete="off"></div>
      <div class="field field-wide"><label for="nn-fp">${t("node_fp")}</label><input id="nn-fp" placeholder="${t("node_fp_create_ph")}" autocomplete="off" spellcheck="false"><small>${t("node_fp_hint")}</small></div>
      <div id="nn-errors" class="pr-errors" hidden></div>
    </div>
    <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("cancel")}</button><button class="btn primary" data-act="nodeCreate">${ic("plus", 14)}<span>${t("create")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
  const prevFocus = document.activeElement;
  new MutationObserver((_m, obs) => {
    if (!modal.isConnected) { obs.disconnect(); if (prevFocus && document.contains(prevFocus)) prevFocus.focus(); }
  }).observe(document.body, { childList: true, subtree: true });
  const validate = () => {
    const errs = [];
    if (($("#nn-name").value.trim().length || 0) < 2) errs.push(t("node_name_req"));
    if (!$("#nn-url").value.trim()) errs.push(t("node_url_req"));
    const fp = $("#nn-fp").value.trim().replace(/[:\s]/g, "");
    if (fp && !/^[0-9a-fA-F]{64}$/.test(fp)) errs.push(t("node_fp_bad"));
    const box = $("#nn-errors");
    const btn = modal.querySelector('[data-act="nodeCreate"]');
    if (errs.length) { box.hidden = false; box.innerHTML = errs.map((e) => `<div class="pr-problem">${ic("alert", 12)}${esc(e)}</div>`).join(""); btn.disabled = true; }
    else { box.hidden = true; box.innerHTML = ""; btn.disabled = false; }
  };
  modal.addEventListener("input", (e) => { if (e.target.closest("#nn-name, #nn-url, #nn-fp")) validate(); });
  $("#nn-name").focus();
  validate();
}
async function nodeCreate(btn) {
  try {
    const tags = ($("#nn-tags")?.value || "").split(",").map((s) => s.trim()).filter(Boolean);
    const fp = ($("#nn-fp")?.value || "").trim().replace(/[:\s]/g, "");
    const body = {
      name: $("#nn-name").value.trim(),
      public_url: $("#nn-url").value.trim(),
      location: $("#nn-location").value.trim(),
      tags,
    };
    if (fp) body.expected_fingerprint = fp;
    const r = await api("/nodes", { method: "POST", body: JSON.stringify(body) });
    const old = btn ? btn.closest(".modal") : null;
    (old || $(".modal"))?.remove();
    const cmd = `./voltd join ${location.origin} ${r.enrollment_token} --public-url ${r.node.public_url}${location.protocol === "http:" ? " --allow-http" : ""}`;
    const m = document.createElement("div");
    m.className = "modal";
    m.innerHTML = `<div class="modal-card node-enroll-modal">
      <div class="modal-head"><div><b>${ic("terminal", 15)} ${t("node_enroll_title")}</b><span class="modal-subtitle">${t("node_enroll_sub")}</span></div><button class="icon-btn" data-act="closeModal" aria-label="${t("close")}">${ic("x", 16)}</button></div>
      <div class="modal-pad-lg"><div class="code-block enroll-cmd">${esc(cmd)}</div></div>
      <div class="modal-foot"><button class="btn ghost" data-act="closeModal">${t("close")}</button><button class="btn primary" data-act="copyText" data-text="${esc(cmd)}">${ic("copy", 14)}<span>${t("node_copy_cmd")}</span></button></div>
    </div>`;
    document.body.appendChild(m);
    adminNodes();
  } catch (e) { toast(e.message, "error"); }
}
async function nodeTest(id){try{const r=await api(`/nodes/${id}/test`,{method:'POST'});toast(`Agent online · ${r.latency_ms}ms`,'success')}catch(e){toast(e.message,'error')}}
async function nodeReenroll(id){try{const r=await api(`/nodes/${id}/enrollment`,{method:'POST'});await vpPrompt('Enrollment token', r.enrollment_token);adminNodes()}catch(e){toast(e.message,'error')}}
async function nodeDelete(id, name) {
  if (!await vpDestroy({
    kind: "node", target: name || `node-${id}`, confirmText: "Detach agent",
    consequences: ["Workloads on this agent become unreachable", "The agent must be re-enrolled to return", "Panel state for its servers is kept"],
  })) return;
  try { await api(`/nodes/${id}`, { method: 'DELETE' }); adminNodes(); } catch (e) { toast(e.message, 'error'); }
}
/* ============================================================
   Fabric node detail — GET /api/nodes/:id returns
   { node, online, available_memory_mb, available_disk_mb,
     drain: {active,mode,reason,deadline,affected_count}, events:[...] }.
   Edit writes PATCH /api/nodes/:id (full-replace UpdateNodeRequest);
   drain writes POST/DELETE /api/nodes/:id/drain. `stop` drain failures
   come back as failed_ids and render in a dedicated box; drain actions
   update in place so half-typed edits in the form are never lost.
   ============================================================ */
async function adminNodeDetail(id) {
  const token = routeToken;
  $("#admin-body").innerHTML = `<div class="empty">${ic("globe", 40)}<p>${t("loading")}</p></div>`;
  let res;
  try { res = await api(`/nodes/${id}`); }
  catch (e) {
    if (token !== routeToken) return;
    const box = $("#admin-body");
    if (box) box.innerHTML = `<div class="file-list"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_load_agents")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminNodeRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
    return;
  }
  if (token !== routeToken) return;
  renderNodeDetail(id, res);
}

function renderNodeDetail(id, res) {
  const n = res.node || {};
  const online = !!res.online;
  const drain = res.drain || {};
  const events = res.events || [];
  const pill = (cls, label) => `<span class="pill ${cls}"><i></i>${esc(label)}</span>`;
  $("#admin-body").innerHTML = `
    <div class="squad-detail-head">
      <a href="#/admin/nodes" class="btn sm ghost">${ic("chevron_left", 13)}<span>${t("back")}</span></a>
      <h2>${ic("globe", 18)} ${esc(n.name || `node-${id}`)}</h2>
      ${online ? pill("running", "online") : pill("offline", "offline")}
      ${drain.active ? `<span class="pill warn"><i></i>${esc(t("node_drain_active").replace("{mode}", drain.mode))}</span>` : ""}
      <div class="actions">
        <button class="icon-btn sm" title="${t("a_test_node")}" aria-label="${t("a_test_node")}" data-act="nodeTest" data-id="${id}">${ic("activity", 15)}</button>
        <button class="icon-btn sm" title="${t("a_reenroll")}" aria-label="${t("a_reenroll")}" data-act="nodeReenroll" data-id="${id}">${ic("key", 15)}</button>
        <button class="icon-btn sm danger" title="${t("a_delete_node")}" aria-label="${t("a_delete_node")}" data-act="nodeDelete" data-id="${id}" data-name="${esc(n.name || "")}">${ic("trash", 15)}</button>
      </div>
    </div>
    <div class="card">
      <div class="card-head"><h3>${ic("info", 15)} ${t("node_identity")}</h3></div>
      <div class="nd-grid">
        <div><span>${t("node_url")}</span><code>${esc(n.public_url || "—")}</code></div>
        <div><span>${t("node_location")}</span><b>${esc(n.location || "—")}</b></div>
        <div><span>${t("node_host")}</span><b>${esc(n.hostname || "—")}</b></div>
        <div><span>${t("node_os_arch")}</span><b>${esc(n.os || "—")} / ${esc(n.arch || "—")}</b></div>
        <div><span>${t("node_agent_version")}</span><b>${esc(n.daemon_version || "—")}</b></div>
        <div><span>${t("node_heartbeat")}</span><b>${esc(n.last_heartbeat ? fmtDate(n.last_heartbeat) : t("node_never_hb"))}</b></div>
        <div class="nd-wide"><span>${t("node_tls_fp")}</span><code class="fp">${esc(n.tls_fingerprint || "—")}</code></div>
        ${n.last_error ? `<div class="nd-wide nd-err"><span>${t("node_last_error")}</span><b>${esc(n.last_error)}</b></div>` : ""}
      </div>
    </div>
    <div class="card" id="nd-drain-card">
      <div class="card-head"><h3>${ic("shield", 15)} ${t("node_drain")}</h3>
        <span class="pill ${drain.active ? "warn" : "plain"}" id="nd-drain-state">${ic("shield", 11)}<span>${drain.active ? esc(t("node_drain_active").replace("{mode}", drain.mode)) : t("gate_none")}</span></span>
      </div>
      ${drain.active ? `<div class="drain-info"><code>${esc(drain.reason || "—")}</code>${drain.deadline ? ` · ${esc(fmtDate(drain.deadline))}` : ""} · ${+drain.affected_count || 0} server(s)</div>` : ""}
      <div class="field"><label>${t("node_drain_reason")}</label><input id="nd-drain-reason" maxlength="512" placeholder="${esc(t("node_drain_reason_ph"))}"${drain.active ? ` value="${esc(drain.reason || "")}"` : ""}></div>
      <div class="field"><label>${t("node_drain_deadline")}</label><input id="nd-drain-deadline" type="number" min="0" step="1" placeholder="${esc(t("node_drain_deadline_ph"))}"></div>
      <div class="drain-modes">
        <button class="btn sm" data-act="nodeDrain" data-mode="hold" data-id="${id}">${ic("pause", 13)}<span>${t("node_drain_hold")}</span></button>
        <button class="btn sm danger" data-act="nodeDrain" data-mode="stop" data-id="${id}">${ic("stop", 13)}<span>${t("node_drain_stop")}</span></button>
        ${drain.active ? `<button class="btn sm ghost" data-act="nodeDrainClear" data-id="${id}">${ic("refresh_ccw", 13)}<span>${t("node_drain_clear")}</span></button>` : ""}
      </div>
      <p class="muted nd-drain-hint">${esc(t("node_drain_hold_hint"))} ${esc(t("node_drain_stop_hint"))}</p>
      <div id="nd-failed" hidden></div>
    </div>
    <div class="card">
      <div class="card-head"><h3>${ic("pencil", 15)} ${t("node_edit")}</h3></div>
      <p class="muted">${esc(t("node_edit_hint"))}</p>
      <div class="modal-form-grid">
        <div class="field"><label>${t("name")}</label><input id="nd-name" maxlength="80" value="${esc(n.name || "")}"></div>
        <div class="field"><label>${t("node_url")}</label><input id="nd-url" value="${esc(n.public_url || "")}"></div>
        <div class="field"><label>${t("node_location")}</label><input id="nd-location" maxlength="80" value="${esc(n.location || "")}"></div>
        <div class="field"><label>${t("node_tags")}</label><input id="nd-tags" placeholder="${esc(t("node_tags_ph"))}" value="${esc((n.tags || []).join(", "))}"></div>
      </div>
      <h4 class="nd-sub">${t("node_limits")}</h4>
      <div class="modal-form-grid">
        <div class="field"><label>${t("node_mem_limit")}</label><input id="nd-mem" type="number" min="0" value="${+n.memory_limit_mb || 0}"></div>
        <div class="field"><label>${t("node_disk_limit")}</label><input id="nd-disk" type="number" min="0" value="${+n.disk_limit_mb || 0}"></div>
        <div class="field"><label>${t("node_mem_over")}</label><input id="nd-memover" type="number" min="0" value="${+n.memory_overallocate || 0}"></div>
        <div class="field"><label>${t("node_disk_over")}</label><input id="nd-diskover" type="number" min="0" value="${+n.disk_overallocate || 0}"></div>
      </div>
      <h4 class="nd-sub">${t("node_sched")}</h4>
      <label class="check-row"><input type="checkbox" id="nd-enabled"${n.enabled ? " checked" : ""}><span class="check-box">${ic("check", 13, 2.4)}</span><span>${t("node_enabled")}</span></label>
      <label class="check-row"><input type="checkbox" id="nd-sched"${n.schedulable ? " checked" : ""}><span class="check-box">${ic("check", 13, 2.4)}</span><span>${t("node_schedulable")}</span></label>
      <label class="check-row"><input type="checkbox" id="nd-maint"${n.maintenance ? " checked" : ""}><span class="check-box">${ic("check", 13, 2.4)}</span><span>${t("node_maintenance")}</span></label>
      <div class="field"><label>${t("node_expected_fp")}</label><input id="nd-fp" class="fp-input" placeholder="${esc(t("node_fp_ph"))}" value="${esc(n.expected_fingerprint || "")}" spellcheck="false"></div>
      <div class="modal-foot nd-save"><button class="btn primary" data-act="adminNodeSave" data-id="${id}">${ic("save", 14)}<span>${t("save")}</span></button></div>
    </div>
    <div class="card">
      <div class="card-head"><h3>${ic("activity", 15)} ${t("node_events")} <span class="badge">${events.length}</span></h3></div>
      <div id="nd-events-list">${renderNodeEvents(events)}</div>
    </div>`;
}

function renderNodeEvents(events) {
  const evLevel = (l) => (l === "error" ? "error" : l === "warn" ? "warn" : "info");
  return (events || []).length
    ? `<div class="node-timeline">${(events || []).map((ev) => `<div class="node-event">
        <span class="pill ${evLevel(ev.level)} plain">${esc(ev.kind || ev.level)}</span>
        <span class="node-event-msg">${esc(ev.message || "")}</span>
        <time>${fmtDate(ev.created_at)}</time>
      </div>`).join("")}</div>`
    : `<div class="empty">${ic("activity", 32)}<p>${t("node_no_events")}</p></div>`;
}

async function nodeRefreshEvents(id) {
  try {
    const r = await api(`/nodes/${id}`);
    const box = $("#nd-events-list");
    if (box) box.innerHTML = renderNodeEvents(r.events || []);
  } catch (e) { toast(e.message, "error"); }
}

async function adminNodeSave(id, btn) {
  const body = {
    name: ($("#nd-name")?.value || "").trim(),
    public_url: ($("#nd-url")?.value || "").trim(),
    enabled: !!$("#nd-enabled")?.checked,
    schedulable: !!$("#nd-sched")?.checked,
    maintenance: !!$("#nd-maint")?.checked,
    location: ($("#nd-location")?.value || "").trim(),
    tags: ($("#nd-tags")?.value || "").split(",").map((s) => s.trim()).filter(Boolean),
    memory_limit_mb: +($("#nd-mem")?.value || 0) || 0,
    disk_limit_mb: +($("#nd-disk")?.value || 0) || 0,
    memory_overallocate: +($("#nd-memover")?.value || 0) || 0,
    disk_overallocate: +($("#nd-diskover")?.value || 0) || 0,
    expected_fingerprint: ($("#nd-fp")?.value || "").trim() || null,
  };
  if (body.name.length < 2) { toast(t("e_node_short"), "error"); return; }
  btn.disabled = true;
  try {
    await api(`/nodes/${id}`, { method: "PATCH", body: JSON.stringify(body) });
    toast(t("node_saved"), "success");
    adminNodeDetail(id);
  } catch (e) { toast(e.message, "error"); }
  btn.disabled = false;
}

async function nodeDrain(id, mode, btn) {
  const reason = ($("#nd-drain-reason")?.value || "").trim();
  const hours = +($("#nd-drain-deadline")?.value || "");
  const body = { mode, reason, deadline_secs: Number.isFinite(hours) && hours > 0 ? Math.round(hours * 3600) : null };
  btn.disabled = true;
  try {
    const r = await api(`/nodes/${id}/drain`, { method: "POST", body: JSON.stringify(body) });
    toast(t("node_drain_started"), "success");
    const st = $("#nd-drain-state");
    if (st && r.drain?.active) { st.className = "pill warn"; st.innerHTML = `${ic("pause", 11)}<span>${esc(t("node_drain_active").replace("{mode}", r.drain.mode))}</span>`; }
    const failed = $("#nd-failed");
    if (failed) {
      const ids = r.failed_ids || [];
      failed.hidden = !ids.length;
      failed.innerHTML = ids.length ? `<div class="drain-failed">${ic("alert", 16)}<div><b>${esc(t("node_drain_failed"))} (${ids.length})</b><code>${esc(ids.join(", "))}</code></div></div>` : "";
    }
    /* A drain set from this view needs its Lift button even when the page
       was loaded with no drain: render it once, next to the hold/stop pair. */
    if (r.drain?.active && !$("#nd-drain-card [data-act='nodeDrainClear']")) {
      const modes = $("#nd-drain-card .drain-modes");
      if (modes) {
        const b = document.createElement("button");
        b.className = "btn sm ghost";
        b.dataset.act = "nodeDrainClear";
        b.dataset.id = String(id);
        b.innerHTML = `${ic("refresh_ccw", 13)}<span>${esc(t("node_drain_clear"))}</span>`;
        modes.appendChild(b);
      }
    }
    nodeRefreshEvents(id);
  } catch (e) { toast(e.message, "error"); }
  btn.disabled = false;
}

async function nodeDrainClear(id) {
  if (!await vpConfirm(t("node_drain_clear_confirm"), t("node_drain"))) return;
  try {
    await api(`/nodes/${id}/drain`, { method: "DELETE" });
    toast(t("node_drain_cleared"), "success");
    const st = $("#nd-drain-state");
    if (st) { st.className = "pill plain"; st.innerHTML = `${ic("shield", 11)}<span>${esc(t("gate_none"))}</span>`; }
    const reason = $("#nd-drain-reason"); if (reason) reason.value = "";
    const dl = $("#nd-drain-deadline"); if (dl) dl.value = "";
    const failed = $("#nd-failed"); if (failed) { failed.hidden = true; failed.innerHTML = ""; }
    $("#nd-drain-card [data-act='nodeDrainClear']")?.remove();
    nodeRefreshEvents(id);
  } catch (e) { toast(e.message, "error"); }
}

let obsTimer = null;
async function adminSystem() {
  if (obsTimer) { clearInterval(obsTimer); obsTimer = null; }
  $("#admin-body").innerHTML = `
    <div class="card" id="a-self"><div class="card-head"><h3>${ic("activity", 15)} ${t("obs_title")}</h3><span class="muted" id="obs-since"></span></div><div id="obs-body"><div class="empty">${ic("activity", 40)}<p>${t("loading")}</p></div></div></div>
    <div class="grid cols-4" id="a-node-grid"></div>
    <div class="grid cols-2">
      <div class="card"><h3>${ic("link", 15)} ${t("obs_endpoints")}</h3><div id="a-alloc"><div class="empty">${ic("link", 40)}<p>${t("loading")}</p></div></div></div>
      <div class="card"><h3>${ic("gauge", 15)} ${t("obs_host_resources")}</h3><div id="a-res"></div></div>
    </div>`;
  // Panel self-metrics: its own 5s poller like the other views' pollers,
  // scoped to its card so a self-metrics failure never clobbers the
  // host-resource section. Re-entry (retry action) clears the previous
  // interval first; killPollers clears it on navigation via state.pollers.
  const obsToken = routeToken;
  let busy = false;
  const obsTick = async () => {
    if (busy || obsToken !== routeToken) return;
    busy = true;
    try {
      const res = await api("/metrics/panel");
      if (obsToken !== routeToken) return;
      renderSelf(res.data || res);
    } catch (e) {
      if (obsToken !== routeToken) return;
      const body = $("#obs-body");
      if (body) body.innerHTML = `<div class="file-error" role="status" aria-live="polite">${ic("alert", 26)}<div><b>${t("err_observatory")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminSystemRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div>`;
    } finally { busy = false; }
  };
  obsTick();
  obsTimer = setInterval(obsTick, 5000);
  state.pollers.push(obsTimer);
  try {
    const s = await api("/system/stats");
    $("#a-node-grid").innerHTML = `
      <div class="card stat-card"><span class="stat-ico green">${ic("zap", 20)}</span><div class="stat-label">${t("cpu")}</div><div class="stat-value">${Math.round(s.cpu.usage_percent)}%</div><div class="stat-sub">freq ${Math.round(s.cpu.frequency_mhz / 1000)} GHz</div></div>
      <div class="card stat-card"><span class="stat-ico purple">${ic("memory", 20)}</span><div class="stat-label">${t("ram")}</div><div class="stat-value">${Math.round(s.memory.percent)}%</div><div class="stat-sub">${fmtBytes(s.memory.used * 1024)} / ${fmtBytes(s.memory.total * 1024)}</div></div>
      <div class="card stat-card"><span class="stat-ico yellow">${ic("harddisk", 20)}</span><div class="stat-label">${t("disk")}</div><div class="stat-value">${Math.round(s.disk.percent)}%</div><div class="stat-sub">${fmtBytes(s.disk.used)} / ${fmtBytes(s.disk.total)}</div></div>
      <div class="card stat-card"><span class="stat-ico accent">${ic("clock", 20)}</span><div class="stat-label">${t("uptime")}</div><div class="stat-value">${fmtTime(s.uptime_secs)}</div><div class="stat-sub">${s.processes} ${t("processes")}</div></div>`;
    $("#a-res").innerHTML = `
      <div class="metric-line"><span>Load 1m / 5m / 15m</span><b>${s.load["1"].toFixed(2)} / ${s.load["5"].toFixed(2)} / ${s.load["15"].toFixed(2)}</b></div>
      <div class="metric-line"><span>CPU</span><b>${Math.round(s.cpu.usage_percent)}%</b></div><div class="progress"><div data-w="${Math.min(100, s.cpu.usage_percent)}"></div></div>
      <div class="metric-line"><span>RAM</span><b>${Math.round(s.memory.percent)}%</b></div><div class="progress"><div class="purple" data-w="${Math.min(100, s.memory.percent)}"></div></div>
      <div class="metric-line"><span>Disk</span><b>${Math.round(s.disk.percent)}%</b></div><div class="progress"><div class="yellow" data-w="${Math.min(100, s.disk.percent)}"></div></div>`;
    $$("#a-res .progress > div[data-w]").forEach((el) => { el.style.width = el.dataset.w + "%"; });
    api("/system/allocations").then((alloc) => {
      const allocs = alloc.data || [];
      const box = $("#a-alloc");
      if (box) box.innerHTML = allocs.length ? `<div class="file-list">${allocs.map((a) => `<div class="file-row"><span class="f-icon">${ic("link", 16)}</span><b>${esc(a.port)}</b><span class="f-meta">→ ${esc(a.server || "free")}</span></div>`).join("")}</div>` : `<div class="empty">${ic("link", 40)}<p>${t("none")}</p></div>`;
    }).catch(() => {
      const box = $("#a-alloc");
      if (box) box.innerHTML = `<div class="empty">${ic("alert", 40)}<p>${esc(t("err_observatory"))}</p></div>`;
    });
  } catch (e) {
    const body = $("#admin-body");
    if (body) body.innerHTML = `<div class="card"><div class="file-error">${ic("alert", 26)}<div><b>${t("err_observatory")}</b><span>${esc(e.message)}</span></div><button class="btn sm" data-act="adminSystemRetry">${ic("refresh_ccw", 13)}<span>${t("retry")}</span></button></div></div>`;
  }
}

function renderSelf(m) {
  const r = m.requests || {};
  const perMinute = Array.isArray(r.per_minute) ? r.per_minute : [];
  const body = $("#obs-body");
  if (!body) return; // superseded route or the outer error card replaced us
  const pool = m.pool || {}, sched = m.scheduler || {}, wh = m.webhooks || {}, mirror = m.mirror || {};
  /* scheduler.last_tick_at is unix seconds, omitted server-side when the
     scheduler never ticked — render elapsed time via fmtTime, "—" otherwise. */
  const lastTick = sched.last_tick_at ? fmtTime(Math.floor(Date.now() / 1000) - sched.last_tick_at) : "—";
  const total = r.total || 0, ok = r.ok || 0;
  const rate = perMinute.length ? perMinute[perMinute.length - 1] : 0;
  const errPct = total > 0 ? Math.round(((total - ok) / total) * 100) : 0;
  const sinceEl = $("#obs-since");
  if (sinceEl) sinceEl.textContent = r.since_unix ? `${t("obs_since")} ${fmtDate(new Date(r.since_unix * 1000).toISOString())}` : "";
  body.innerHTML = `
    <div class="grid cols-4 obs-cards">
      <div class="card stat-card"><span class="stat-ico accent">${ic("activity", 20)}</span><div class="stat-label">${t("obs_requests")}</div><div class="stat-value">${total.toLocaleString()}</div><div class="stat-sub">${ok} ${t("obs_ok")} · ${errPct}% ${t("obs_errors")}</div></div>
      <div class="card stat-card"><span class="stat-ico green">${ic("zap", 20)}</span><div class="stat-label">${t("obs_rate")}</div><div class="stat-value">${rate}</div><div class="stat-sub">${t("obs_last")} ${perMinute.length} ${t("obs_minutes")}</div></div>
      <div class="card stat-card"><span class="stat-ico purple">${ic("database", 20)}</span><div class="stat-label">${t("obs_pool")}</div><div class="stat-value">${pool.connections ?? "—"} / ${pool.max ?? "—"}</div><div class="stat-sub">${t("obs_idle")} ${pool.idle ?? "—"}${pool.saturated ? ` · <span class="pill warn plain">${esc(t("obs_pool_saturated"))}</span>` : ""}</div></div>
      <div class="card stat-card"><span class="stat-ico yellow">${ic("clock", 20)}</span><div class="stat-label">${t("obs_uptime")}</div><div class="stat-value">${fmtTime(m.uptime_secs)}</div><div class="stat-sub">${esc(mirror.status || "")} ${t("obs_mirror")}</div></div>
    </div>
    <div class="obs-rate-head"><span class="muted">${ic("activity", 12)} ${t("obs_rate")} · ${t("obs_last")} ${perMinute.length} ${t("obs_minutes")}</span></div>
    <div class="sparkline">${sparkSvg(perMinute, "accent", `role="img" aria-label="${perMinute.map(Number).join(", ")}"`)}</div>
    <div class="grid cols-2">
      <div class="metric-line"><span>${t("obs_sched")}</span><b>${sched.pending_runs ?? "—"} ${t("obs_pending")} · ${t("obs_last_tick")} ${lastTick}</b></div>
      <div class="metric-line"><span>${t("obs_webhooks")}</span><b>${wh.pending_deliveries ?? "—"} ${t("obs_pending")}</b></div>
    </div>`;
}
