#!/usr/bin/env bash
# Local-only probes: run llm-proxy against a fake upstream on loopback and print
# what it forwarded, returned, and recorded. No real credentials or providers.
# Usage: run.sh /path/to/llm-proxy    (takes about 45 s; the last step checks the process at 35 s)
set -u
BIN=${1:?usage: run.sh /path/to/llm-proxy}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
mkdir -p "$WORK/providers"
cat > "$WORK/config.toml" <<CFG
[server]
bind = "127.0.0.1:28080"
request_timeout = "60s"
log_level = "warn"
hot_reload = false
rate_limit_rpm = 0
trust_forwarded_headers = false
server_name = "llm-proxy"
[auth]
mode = "none"
[storage]
path = "$WORK/usage.db"
CFG
cat > "$WORK/providers/oai.toml" <<'CFG'
[provider]
name = "oai"
api_key = "test-upstream-key-not-real"
auth_style = "bearer"
[provider.adapters.chat]
protocol = "openai_chat_completions"
endpoint = "http://127.0.0.1:28081/chat/completions"
[provider.routes]
chat_completions = "chat"
messages = "chat"
[provider.pricing."deepseek"]
input = "0.000001"
output = "0.000002"
[provider.pricing."oai-cached"]
input = "0.000001"
output = "0.000002"
cache_read = "0.0000001"
CFG
cat > "$WORK/providers/ant.toml" <<'CFG'
[provider]
name = "ant"
api_key = "test-upstream-key-not-real"
auth_style = "x-api-key"
[provider.adapters.anthropic]
protocol = "anthropic_messages"
endpoint = "http://127.0.0.1:28081/v1/messages"
[provider.routes]
messages = "anthropic"
CFG
python3 -I "$HERE/upstream.py" 28081 "$WORK/cap.jsonl" & U1=$!
python3 -I "$HERE/upstream.py" 28082 "$WORK/cap.jsonl" & U2=$!
T0=$(python3 -I -c 'import time;print(time.time())')
"$BIN" serve --config "$WORK/config.toml" > "$WORK/proxy.log" 2>&1 & P=$!
python3 -I -c 'import time;time.sleep(1.2)'
python3 -I "$HERE/probe_fields.py" "$WORK/cap.jsonl" "$WORK/usage.db"
echo; python3 -I "$HERE/probe_streams.py" "$WORK/usage.db" "$T0"
python3 -I -c "import time;time.sleep(max(0,$T0+35-time.time()))"
if kill -0 $P 2>/dev/null; then
  echo "proxy still running 35 s after start; sending SIGTERM"
  kill -TERM $P
else
  echo "proxy already exited before 35 s"
fi
wait $P; echo "proxy exit code=$?"
grep -E 'deadline' "$WORK/proxy.log" | tail -1 | cut -c1-200
kill $U1 $U2 2>/dev/null
echo "work dir: $WORK"
