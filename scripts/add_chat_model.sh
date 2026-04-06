#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 <config_file>" >&2
  exit 1
fi

CONFIG_FILE="$1"
LATEST_VERSION="0.5"

if [[ ! -f "$CONFIG_FILE" ]]; then
  echo "Config file not found: $CONFIG_FILE" >&2
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "This script requires jq." >&2
  exit 1
fi

CONFIG_VERSION="$(jq -r '.version // "0.1"' "$CONFIG_FILE")"
if [[ "$CONFIG_VERSION" != "$LATEST_VERSION" ]]; then
  echo "This script only supports saved config version $LATEST_VERSION." >&2
  echo "Current file version is $CONFIG_VERSION." >&2
  echo "Please upgrade the config file to the latest saved format first, then rerun this script." >&2
  exit 1
fi

if ! jq -e '.models.chat | type == "object"' "$CONFIG_FILE" >/dev/null 2>&1; then
  echo "This script expects a latest-version config with .models.chat." >&2
  exit 1
fi

if ! command -v codex >/dev/null 2>&1; then
  CODEX_AVAILABLE=0
else
  CODEX_AVAILABLE=1
fi

TMP_DIR="$(mktemp -d)"
MODEL_JSON_FILE="$TMP_DIR/model.json"
UPDATED_CONFIG_FILE="$TMP_DIR/config.json"
trap 'rm -rf "$TMP_DIR"' EXIT

trim() {
  local value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "$value"
}

expand_home() {
  local path="$1"
  if [[ "$path" == "~/"* ]]; then
    printf '%s\n' "$HOME/${path#"~/"}"
  else
    printf '%s\n' "$path"
  fi
}

prompt_text() {
  local prompt="$1"
  local default_value="${2-}"
  local value=""
  if [[ -n "$default_value" ]]; then
    read -r -p "$prompt [$default_value]: " value
    value="$(trim "$value")"
    if [[ -z "$value" ]]; then
      value="$default_value"
    fi
  else
    read -r -p "$prompt: " value
    value="$(trim "$value")"
  fi
  printf '%s\n' "$value"
}

prompt_required() {
  local prompt="$1"
  local default_value="${2-}"
  local value=""
  while true; do
    value="$(prompt_text "$prompt" "$default_value")"
    if [[ -n "$value" ]]; then
      printf '%s\n' "$value"
      return 0
    fi
    echo "This field is required."
  done
}

prompt_yes_no() {
  local prompt="$1"
  local default_value="${2:-n}"
  local value=""
  local normalized_default
  normalized_default="$(printf '%s' "$default_value" | tr '[:upper:]' '[:lower:]')"
  while true; do
    if [[ "$normalized_default" == "y" ]]; then
      read -r -p "$prompt [Y/n]: " value
    else
      read -r -p "$prompt [y/N]: " value
    fi
    value="$(trim "$value")"
    if [[ -z "$value" ]]; then
      value="$normalized_default"
    fi
    value="$(printf '%s' "$value" | tr '[:upper:]' '[:lower:]')"
    case "$value" in
      y|yes) printf 'true\n'; return 0 ;;
      n|no) printf 'false\n'; return 0 ;;
    esac
    echo "Please answer y or n."
  done
}

prompt_optional_json() {
  local prompt="$1"
  local value=""
  while true; do
    read -r -p "$prompt (leave blank for none): " value
    value="$(trim "$value")"
    if [[ -z "$value" ]]; then
      printf '\n'
      return 0
    fi
    if printf '%s' "$value" | jq -e . >/dev/null 2>&1; then
      printf '%s\n' "$value"
      return 0
    fi
    echo "Invalid JSON. Try again."
  done
}

choose_provider() {
  while true; do
    echo >&2
    echo "Choose provider:" >&2
    echo "  1) codex" >&2
    echo "  2) openrouter" >&2
    echo "  3) openrouter (responses)" >&2
    local choice=""
    read -r -p "Enter 1/2/3: " choice >&2
    choice="$(trim "$choice")"
    case "$choice" in
      1)
        if [[ "$CODEX_AVAILABLE" -eq 0 ]]; then
          echo "codex CLI is not available in PATH." >&2
        else
          printf 'codex-subscription\n'
          return 0
        fi
        ;;
      2) printf 'openrouter\n'; return 0 ;;
      3) printf 'openrouter-resp\n'; return 0 ;;
    esac
  done
}

MODEL_TYPE="$(choose_provider)"
API_ENDPOINT=""
API_KEY_ENV=""
CODEX_HOME_VALUE=""
DEFAULT_MODEL_NAME=""
DEFAULT_SUPPORTS_VISION="n"
DEFAULT_TIMEOUT_SECONDS="300"
DEFAULT_CONTEXT_WINDOW_TOKENS="262144"
DEFAULT_CACHE_TTL=""

case "$MODEL_TYPE" in
  openrouter|openrouter-resp)
    API_ENDPOINT="https://openrouter.ai/api/v1"
    API_KEY_ENV="OPENROUTER_API_KEY"
    DEFAULT_SUPPORTS_VISION="y"
    ;;
  codex-subscription)
    API_ENDPOINT="https://chatgpt.com/backend-api/codex"
    CODEX_HOME_VALUE="${CODEX_HOME:-$HOME/.codex}"
    CODEX_HOME_VALUE="$(expand_home "$CODEX_HOME_VALUE")"
    DEFAULT_MODEL_NAME="gpt-5"
    DEFAULT_SUPPORTS_VISION="y"
    if [[ ! -f "$CODEX_HOME_VALUE/auth.json" ]]; then
      echo >&2
      echo "No saved Codex login found." >&2
      echo "Starting: codex login --device-auth" >&2
      echo "Please finish the browser/device-auth flow shown by codex. The script will continue automatically after login succeeds." >&2
      CODEX_HOME="$CODEX_HOME_VALUE" codex login --device-auth
    else
      echo "Reusing existing Codex login." >&2
    fi
    if [[ ! -f "$CODEX_HOME_VALUE/auth.json" ]]; then
      echo "Login did not produce $CODEX_HOME_VALUE/auth.json." >&2
      exit 1
    fi
    echo "No API key is needed for Codex subscription. Next I only need the local model alias and model metadata." >&2
    ;;
esac

MODEL_KEY="$(prompt_required "Local model alias (config key, not API key)")"

if jq -e --arg key "$MODEL_KEY" '.models.chat[$key] != null' "$CONFIG_FILE" >/dev/null 2>&1; then
  OVERWRITE="$(prompt_yes_no "Model '$MODEL_KEY' already exists. Overwrite it?" "n")"
  if [[ "$OVERWRITE" != "true" ]]; then
    echo "Aborted."
    exit 1
  fi
fi

MODEL_NAME="$(prompt_required "Provider model id" "$DEFAULT_MODEL_NAME")"
DESCRIPTION="$(prompt_required "Short description")"
SUPPORTS_VISION_INPUT="$(prompt_yes_no "Supports vision input?" "$DEFAULT_SUPPORTS_VISION")"
USE_DEFAULTS="$(prompt_yes_no "Use default timeout/context/provider settings?" "y")"
TIMEOUT_SECONDS="$DEFAULT_TIMEOUT_SECONDS"
CONTEXT_WINDOW_TOKENS="$DEFAULT_CONTEXT_WINDOW_TOKENS"
CACHE_TTL="$DEFAULT_CACHE_TTL"
IMAGE_TOOL_MODEL=""
WEB_SEARCH_MODEL=""
ENABLE_NATIVE_WEB_SEARCH="false"
NATIVE_WEB_SEARCH_PAYLOAD=""

if [[ "$USE_DEFAULTS" != "true" ]]; then
  TIMEOUT_SECONDS="$(prompt_required "timeout_seconds" "$DEFAULT_TIMEOUT_SECONDS")"
  CONTEXT_WINDOW_TOKENS="$(prompt_required "context_window_tokens" "$DEFAULT_CONTEXT_WINDOW_TOKENS")"
  CACHE_TTL="$(prompt_text "cache_ttl" "$DEFAULT_CACHE_TTL")"
  if [[ "$MODEL_TYPE" == "openrouter" || "$MODEL_TYPE" == "openrouter-resp" ]]; then
    API_ENDPOINT="$(prompt_text "api_endpoint" "$API_ENDPOINT")"
    API_KEY_ENV="$(prompt_text "api_key_env" "$API_KEY_ENV")"
  fi
  IMAGE_TOOL_MODEL="$(prompt_text "image_tool_model (self / another model key)" "")"
  WEB_SEARCH_MODEL="$(prompt_text "web_search alias (name from models.web_search)" "")"
  ENABLE_NATIVE_WEB_SEARCH="$(prompt_yes_no "Enable native_web_search?" "n")"
  if [[ "$ENABLE_NATIVE_WEB_SEARCH" == "true" ]]; then
    NATIVE_WEB_SEARCH_PAYLOAD="$(prompt_optional_json "native_web_search.payload JSON")"
  fi
fi

jq -n \
  --arg type "$MODEL_TYPE" \
  --arg model "$MODEL_NAME" \
  --arg description "$DESCRIPTION" \
  --arg api_endpoint "$API_ENDPOINT" \
  --arg api_key_env "$API_KEY_ENV" \
  --arg codex_home "$CODEX_HOME_VALUE" \
  --arg timeout_seconds "$TIMEOUT_SECONDS" \
  --arg context_window_tokens "$CONTEXT_WINDOW_TOKENS" \
  --arg cache_ttl "$CACHE_TTL" \
  --arg image_tool_model "$IMAGE_TOOL_MODEL" \
  --arg web_search_model "$WEB_SEARCH_MODEL" \
  --arg supports_vision_input "$SUPPORTS_VISION_INPUT" \
  --arg enable_native_web_search "$ENABLE_NATIVE_WEB_SEARCH" \
  --arg native_web_search_payload "$NATIVE_WEB_SEARCH_PAYLOAD" \
  '
  {
    type: $type,
    model: $model,
    description: $description,
    supports_vision_input: ($supports_vision_input == "true"),
    timeout_seconds: ($timeout_seconds | tonumber),
    context_window_tokens: ($context_window_tokens | tonumber)
  }
  + (if $api_endpoint == "" then {} else {api_endpoint: $api_endpoint} end)
  + (if $api_key_env == "" then {} else {api_key_env: $api_key_env} end)
  + (if $codex_home == "" then {} else {codex_home: $codex_home} end)
  + (if $cache_ttl == "" then {} else {cache_ttl: $cache_ttl} end)
  + (if $image_tool_model == "" then {} else {image_tool_model: $image_tool_model} end)
  + (if $web_search_model == "" then {} else {web_search: $web_search_model} end)
  + (
      if $enable_native_web_search == "true" then
        {
          native_web_search: {
            enabled: true,
            payload: (
              if $native_web_search_payload == "" then
                {}
              else
                ($native_web_search_payload | fromjson)
              end
            )
          }
        }
      else
        {}
      end
    )
  ' > "$MODEL_JSON_FILE"

echo
echo "Model to write:"
jq . "$MODEL_JSON_FILE"
echo

CONFIRM="$(prompt_yes_no "Write this model into $CONFIG_FILE?" "y")"
if [[ "$CONFIRM" != "true" ]]; then
  echo "Aborted."
  exit 1
fi

jq --arg key "$MODEL_KEY" --slurpfile model "$MODEL_JSON_FILE" \
  '.models.chat[$key] = $model[0]' \
  "$CONFIG_FILE" > "$UPDATED_CONFIG_FILE"

mv "$UPDATED_CONFIG_FILE" "$CONFIG_FILE"
echo "Added chat model '$MODEL_KEY' to $CONFIG_FILE"
