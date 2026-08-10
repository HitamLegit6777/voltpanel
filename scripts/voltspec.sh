#!/usr/bin/env bash
# voltspec.sh — VoltSpec Registry CLI.
#
# Thin curl+jq wrapper over the panel's VoltSpec Registry API. The endpoints
# ship with the panel; this script adds no backend behavior, it only formats
# requests and surfaces the server's JSON `.error` envelope on failure.
#
# Environment:
#   VOLTPANEL_URL      panel base URL        (default: http://127.0.0.1:8080)
#   VOLTPANEL_API_KEY  API token (vp_...)    (required for every command)
set -Eeuo pipefail

usage() {
  cat <<'EOF'
VoltSpec Registry CLI

Usage:
  voltspec.sh <command> [args]

Commands:
  status                    Show signing posture (GET /api/settings/registry)
  key generate              Generate a fresh signing key (the seed prints once)
  key set <hex>             Set the signing key to a 64-hex-char seed
  key clear                 Disable signing
  list                      List published packages + local installs
  publish <blueprint-id>    Publish the latest revision of a blueprint
  install <id>[@version]    Install a package (defaults to newest version)
  package <id> <version>    Fetch a package document to stdout
  fetch <id> <version>      Download a package to <id>@<version>.json
                            (use -o <outfile> to override)
  help, --help, -h          Show this help

Environment:
  VOLTPANEL_URL      Panel base URL (default: http://127.0.0.1:8080)
  VOLTPANEL_API_KEY  API token (vp_...), required for every command

Every command exits non-zero on request or API error; the server's `.error`
message is printed when the response carries one.
EOF
}

# request <method> <path> [json-body] [outfile] — perform an authenticated API
# call and print the response body on success, or write it to outfile (without
# printing) when outfile is given. On transport failure or a non-2xx response
# (surfacing .error from the JSON envelope) it exits non-zero.
#
# With an outfile, the body is staged in a unique temp file next to the final
# path and renamed into place only on success (overwriting as the caller
# expects). A failed request removes only the temp file — a pre-existing file
# at the target path is never touched.
#
# The API token, any request body (e.g. a signing-key seed) and the optional
# outfile are handed to curl through a stdin config file (`-K -`), never as
# argv, so they cannot be read from ps/proc/<pid>/cmdline by other same-host
# users.
request() {
  local method=$1 path=$2 data=${3-} outfile=${4-}
  local body_file='' code config target
  if [[ -n $outfile ]]; then
    target=$(mktemp "${outfile}.tmp.XXXXXX") || {
      printf 'error: could not create a temporary file\n' >&2
      exit 1
    }
  else
    body_file=$(mktemp) || {
      printf 'error: could not create a temporary file\n' >&2
      exit 1
    }
    target=$body_file
  fi
  config=$(printf '%s\n' \
    'silent' \
    'show-error' \
    'max-time = 30' \
    "request = $method" \
    "header = \"Authorization: Bearer $VOLTPANEL_API_KEY\"" \
    "output = $target" \
    'write-out = "%{http_code}"')
  if [[ -n $data ]]; then
    config+=$'\nheader = "Content-Type: application/json"'
    config+=$'\ndata = '"$data"
  fi
  config+=$'\nurl = '"\"$VOLTPANEL_URL$path\""
  code=$(printf '%s\n' "$config" | curl -K -) || {
    rm -f "$target"
    printf 'error: request to %s failed (panel unreachable? check VOLTPANEL_URL)\n' "$path" >&2
    exit 1
  }
  if [[ $code -lt 200 || $code -ge 300 ]]; then
    local msg
    msg=$(jq -r '.error // empty' "$target" 2>/dev/null || true)
    rm -f "$target"
    if [[ -n $msg ]]; then
      printf 'error: %s (HTTP %s)\n' "$msg" "$code" >&2
    else
      printf 'error: HTTP %s from %s\n' "$code" "$path" >&2
    fi
    exit 1
  fi
  if [[ -n $outfile ]]; then
    mv -f "$target" "$outfile" || {
      rm -f "$target"
      printf 'error: could not write %s\n' "$outfile" >&2
      exit 1
    }
    return 0
  fi
  printf '%s\n' "$(<"$target")"
  rm -f "$body_file"
}

# valid_package_id <id> — true if the id is a safe URL path segment: non-empty
# and free of path separators, traversal dots, URL/query metacharacters,
# quotes, backslashes and whitespace.
valid_package_id() {
  local id=$1
  [[ -n $id && $id != *'..'* && $id != */* && $id != *'?'* && $id != *'#'* \
    && $id != *'"'* && $id != *'\'* && $id != *[[:space:]]* ]]
}

# newest_registry_version <id> — resolve the newest published version of a
# package from the registry catalog, for `install <id>` without @version.
newest_registry_version() {
  local id=$1 catalog version
  catalog=$(request GET /api/blueprints/registry) || return 1
  version=$(printf '%s' "$catalog" | jq -r --arg id "$id" '
    .data.packages
    | map(select(.id == $id))
    | if length > 0 then max_by(.version).version else empty end
  ')
  if [[ -z $version ]]; then
    printf 'error: package %s not found in the registry catalog\n' "$id" >&2
    return 1
  fi
  printf '%s' "$version"
}

cmd_status() {
  request GET /api/settings/registry | jq '.'
}

cmd_key() {
  local action=${1-}
  case $action in
    generate)
      request POST /api/settings/registry/signing-key '{"key":null}' | jq '.'
      printf 'note: a freshly generated seed is shown only once above — store it now.\n' >&2
      ;;
    set)
      local hex=${2-}
      [[ -n $hex ]] || {
        printf 'error: usage: voltspec.sh key set <hex>\n' >&2
        exit 1
      }
      if [[ ! $hex =~ ^[0-9a-fA-F]{64}$ ]]; then
        printf 'error: signing key must be exactly 64 hex characters (32 bytes)\n' >&2
        exit 1
      fi
      request POST /api/settings/registry/signing-key "$(jq -nc --arg key "$hex" '{key: $key}')" | jq '.'
      ;;
    clear)
      request POST /api/settings/registry/signing-key '{"key":""}' | jq '.'
      ;;
    *)
      printf 'error: unknown key action %s (expected generate | set | clear)\n' "${action:-}" >&2
      exit 1
      ;;
  esac
}

cmd_list() {
  request GET /api/blueprints/registry | jq '.data'
}

cmd_publish() {
  local id=${1-}
  if [[ ! $id =~ ^[0-9]+$ ]]; then
    printf 'error: usage: voltspec.sh publish <blueprint-id> (numeric id)\n' >&2
    exit 1
  fi
  request POST /api/blueprints/registry/publish "$(jq -nc --argjson id "$id" '{id: $id}')" | jq '.'
}

cmd_install() {
  local spec=${1-} id version
  [[ -n $spec ]] || {
    printf 'error: usage: voltspec.sh install <id>[@version]\n' >&2
    exit 1
  }
  if [[ $spec == *@* ]]; then
    id=${spec%@*}
    version=${spec##*@}
  else
    id=$spec
    version=$(newest_registry_version "$id") || exit 1
  fi
  if ! valid_package_id "$id"; then
    printf 'error: invalid package id %s\n' "$id" >&2
    exit 1
  fi
  if [[ ! $version =~ ^[0-9]+$ ]]; then
    printf 'error: invalid version %s (expected a non-negative integer)\n' "$version" >&2
    exit 1
  fi
  request POST /api/blueprints/registry/import "$(jq -nc --arg id "$id" --argjson version "$version" '{id: $id, version: $version}')" | jq '.'
}

cmd_package() {
  local id=${1-} version=${2-}
  if ! valid_package_id "$id" || [[ ! $version =~ ^[0-9]+$ ]]; then
    printf 'error: usage: voltspec.sh package <id> <version>\n' >&2
    exit 1
  fi
  request GET "/api/blueprints/registry/package/$id/$version" | jq '.'
}

cmd_fetch() {
  local id=${1-} version=${2-} outfile=
  if [[ -n ${3-} ]]; then
    if [[ $3 == -o ]]; then
      outfile=${4-}
      [[ -n $outfile ]] || {
        printf 'error: -o requires an output file path\n' >&2
        exit 1
      }
      [[ -z ${5-} ]] || {
        printf 'error: unexpected argument: %s\n' "$5" >&2
        exit 1
      }
    else
      printf 'error: unexpected argument: %s\n' "$3" >&2
      exit 1
    fi
  fi
  if ! valid_package_id "$id" || [[ ! $version =~ ^[0-9]+$ ]]; then
    printf 'error: usage: voltspec.sh fetch <id> <version> [-o <outfile>]\n' >&2
    exit 1
  fi
  outfile=${outfile:-"$id@$version.json"}
  request GET "/api/blueprints/registry/package/$id/$version" '' "$outfile"
  printf 'fetched %s to %s\n' "$id@$version" "$outfile" >&2
}

main() {
  local cmd=${1-}
  if [[ -z $cmd || $cmd == -h || $cmd == --help || $cmd == help ]]; then
    usage
    [[ -z $cmd ]] && exit 1 || exit 0
  fi
  command -v jq >/dev/null 2>&1 || {
    printf 'error: jq is required by voltspec.sh but is not installed\n' >&2
    exit 1
  }
  command -v curl >/dev/null 2>&1 || {
    printf 'error: curl is required by voltspec.sh but is not installed\n' >&2
    exit 1
  }
  if [[ -z ${VOLTPANEL_API_KEY:-} ]]; then
    printf 'error: VOLTPANEL_API_KEY is not set (a vp_ API token is required)\n' >&2
    exit 1
  fi
  VOLTPANEL_URL=${VOLTPANEL_URL:-http://127.0.0.1:8080}
  VOLTPANEL_URL=${VOLTPANEL_URL%/}
  shift
  case $cmd in
    status) cmd_status ;;
    key) cmd_key "$@" ;;
    list) cmd_list ;;
    publish) cmd_publish "$@" ;;
    install) cmd_install "$@" ;;
    package) cmd_package "$@" ;;
    fetch) cmd_fetch "$@" ;;
    *)
      printf 'error: unknown command: %s\n' "$cmd" >&2
      usage >&2
      exit 1
      ;;
  esac
}

main "$@"
