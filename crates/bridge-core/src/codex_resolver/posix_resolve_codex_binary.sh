# Find existing `codex` binaries on the remote and emit "codex:<path>" for the
# newest parseable version, or the first executable fallback if none report a
# version. Caller composes this template via the PROFILE_INIT,
# PACKAGE_MANAGER_PROBE, and SHARED_LINES placeholders.
{{PROFILE_INIT}}
_litter_first_selector=""
_litter_first_path=""
_litter_best_selector=""
_litter_best_path=""
_litter_best_major=-1
_litter_best_minor=-1
_litter_best_patch=-1

_litter_consider_candidate() {
  _litter_selector="$1"
  _litter_path="$2"
  if [ -n "$_litter_path" ] && [ -f "$_litter_path" ] && [ -x "$_litter_path" ]; then
    if [ -z "$_litter_first_path" ]; then
      _litter_first_selector="$_litter_selector"
      _litter_first_path="$_litter_path"
    fi
    _litter_version="$("$_litter_path" --version 2>/dev/null | awk 'match($0, /[0-9]+[.][0-9]+[.][0-9]+/) { print substr($0, RSTART, RLENGTH); exit }' | tr . ' ')"
    if [ -n "$_litter_version" ]; then
      _litter_version_ifs="$IFS"
      IFS=' '
      set -- $_litter_version
      IFS="$_litter_version_ifs"
      _litter_major="${1:-0}"
      _litter_minor="${2:-0}"
      _litter_patch="${3:-0}"
      if [ "$_litter_best_major" -lt 0 ] \
        || [ "$_litter_major" -gt "$_litter_best_major" ] \
        || { [ "$_litter_major" -eq "$_litter_best_major" ] && [ "$_litter_minor" -gt "$_litter_best_minor" ]; } \
        || { [ "$_litter_major" -eq "$_litter_best_major" ] && [ "$_litter_minor" -eq "$_litter_best_minor" ] && [ "$_litter_patch" -gt "$_litter_best_patch" ]; }; then
        _litter_best_selector="$_litter_selector"
        _litter_best_path="$_litter_path"
        _litter_best_major="$_litter_major"
        _litter_best_minor="$_litter_minor"
        _litter_best_patch="$_litter_patch"
      fi
    fi
  fi
}
_litter_consider_from_dir() {
  _litter_selector="$1"
  _litter_name="$2"
  _litter_dir="$3"
  if [ -n "$_litter_dir" ]; then
    _litter_consider_candidate "$_litter_selector" "$_litter_dir/$_litter_name"
  fi
}
_litter_consider_path_candidates() {
  _litter_selector="$1"
  _litter_name="$2"
  _litter_old_ifs="$IFS"
  IFS=:
  for _litter_dir in $PATH; do
    if [ -n "$_litter_dir" ]; then
      _litter_consider_candidate "$_litter_selector" "$_litter_dir/$_litter_name"
    fi
  done
  IFS="$_litter_old_ifs"
}
{{SHARED_LINES}}
{{PACKAGE_MANAGER_PROBE}}
_litter_consider_from_dir codex codex "$_litter_bun_global_bin"
_litter_consider_from_dir codex codex "$_litter_npm_global_bin"
_litter_consider_from_dir codex codex "$_litter_pnpm_global_bin"

if [ -n "$_litter_best_path" ]; then
  printf '%s:%s' "$_litter_best_selector" "$_litter_best_path"
  exit 0
fi
if [ -n "$_litter_first_path" ]; then
  printf '%s:%s' "$_litter_first_selector" "$_litter_first_path"
  exit 0
fi
