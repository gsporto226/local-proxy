# Claude Code status line script (PowerShell). Reads the status-line JSON from
# stdin, extracts the session id, and asks local-proxy to render the status
# line for that session. Configure via settings.json:
#
#   "statusLine": { "type": "command", "command": "<abs path>\\scripts\\statusline.ps1" }
#
# The template comes from the proxy config `statusline:` block or is passed
# with --template (flag overrides config).

param(
  [string]$Template = ""
)

$input = [Console]::In.ReadToEnd()

$session = ""
if ($input -match '"session_id"\s*:\s*"([^"]*)"') { $session = $Matches[1] }
$model = ""
if ($input -match '"(display_name|model)"\s*:\s*"([^"]*)"') { $model = $Matches[2] }
$ctx = ""
if ($input -match '"used_percentage"\s*:\s*([0-9.]*)') { $ctx = $Matches[1] }

$args = @()
if ($session) { $args += "--session", $session }
if ($model)   { $args += "--model", $model }
if ($ctx)     { $args += "--context-pct", $ctx }
if ($Template){ $args += "--template", $Template }

& local-proxy statusline @args
