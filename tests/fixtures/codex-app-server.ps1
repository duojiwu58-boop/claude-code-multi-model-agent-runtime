param(
    [string]$Mode = 'normal',
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Remaining
)
$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
function Send-Frame($Frame) {
    [Console]::WriteLine(($Frame | ConvertTo-Json -Depth 60 -Compress))
}
$tools = @()
$history = @()
$callbackResults = @()
$queued = @()
$waiting = $false
$turnNumber = 0
while ($null -ne ($line = [Console]::ReadLine())) {
    $request = $line | ConvertFrom-Json -AsHashtable
    if ($request.ContainsKey('result') -and $request.id -in @(900, 901)) {
        if (-not $Mode.StartsWith('continuation')) { throw 'Unexpected tool callback result' }
        $callbackResults += $request.result
        if ($Mode -eq 'continuation-parallel' -and $callbackResults.Count -lt 2) { continue }
        $text = 'Native result: ' + $callbackResults[-1].contentItems[-1].text
        if ($callbackResults[0].success -eq $false) { $text += ' (error preserved)' }
        if ($queued.Count -gt 0) { $text += ' | appended:' + (($queued | ForEach-Object { $_.role }) -join ',') }
        $waiting = $false
        Send-Frame @{ method = 'item/agentMessage/delta'; params = @{ itemId = 'final'; delta = $text } }
        Send-Frame @{ method = 'item/completed'; params = @{ item = @{ id = 'final'; type = 'agentMessage'; phase = 'final_answer'; text = $text } } }
        Send-Frame @{ method = 'thread/tokenUsage/updated'; params = @{ tokenUsage = @{ total = @{ inputTokens = 220; outputTokens = 25; cachedInputTokens = 40; reasoningOutputTokens = 15 } } } }
        Send-Frame @{ method = 'turn/completed'; params = @{ turn = @{ status = 'completed' } } }
        continue
    }
    $result = @{}
    switch ($request.method) {
        'initialize' { }
        'initialized' { continue }
        'config/read' { $result = @{ config = @{ mcp_servers = @{ 'test.server' = @{} } } } }
        'thread/start' {
            if ($request.params.config['features.shell_tool'] -ne $false -or
                $request.params.config.mcp_servers['test.server'].enabled -ne $false -or
                $request.params.environments.Count -ne 0) {
                throw 'Native tools were not disabled'
            }
            $tools = $request.params.dynamicTools
            $result = @{ thread = @{ id = "mock-thread-$PID" } }
        }
        'thread/inject_items' {
            if ($waiting) { $queued += $request.params.items }
            $history += $request.params.items
        }
        'turn/start' {
            $turnNumber++
            if ($Mode -eq 'session') {
                Send-Frame @{ id = $request.id; result = @{ turn = @{ id = "turn-$turnNumber" } } }
                Send-Frame @{ method = 'item/agentMessage/delta'; params = @{ delta = "Session $PID turn $turnNumber" } }
                Send-Frame @{ method = 'thread/tokenUsage/updated'; params = @{ tokenUsage = @{ total = @{ inputTokens = 20 * $turnNumber; outputTokens = 10 * $turnNumber } } } }
                Send-Frame @{ method = 'turn/completed'; params = @{ turn = @{ status = 'completed' } } }
                continue
            }
            if ($Mode -eq 'timeout') { Start-Sleep -Seconds 30; continue }
            if ($Mode -eq 'rpc-error') {
                Send-Frame @{ id = $request.id; error = @{ message = 'mock unsupported model' } }
                continue
            }
            $outputs = @($history | Where-Object { $_.type -eq 'function_call_output' })
            if ($tools.Count -gt 0 -and $outputs.Count -eq 0) {
                $waiting = $true
                if ($Mode.StartsWith('continuation')) {
                    Send-Frame @{ method = 'thread/tokenUsage/updated'; params = @{ tokenUsage = @{ total = @{ inputTokens = 100; outputTokens = 10; cachedInputTokens = 20; reasoningOutputTokens = 6 } } } }
                }
                # Deliberately arrives before the turn/start acknowledgement.
                $arguments = if ($Mode -eq 'bad-arguments') { 'invalid' } else { @{ file_path = 'sample.txt' } }
                Send-Frame @{ id = 900; method = 'item/tool/call'; params = @{
                    threadId = "mock-thread-$PID"; turnId = 'mock-turn'; callId = 'mock-call';
                    tool = $tools[0].name; arguments = $arguments
                } }
                if ($Mode -eq 'continuation-parallel') {
                    Send-Frame @{ id = 901; method = 'item/tool/call'; params = @{
                        threadId = "mock-thread-$PID"; turnId = 'mock-turn'; callId = 'mock-call-2';
                        tool = $tools[0].name; arguments = @{ file_path = 'second.txt' }
                    } }
                }
                continue
            }
            Send-Frame @{ id = $request.id; result = @{ turn = @{ id = 'mock-turn' } } }
            if ($Mode -eq 'native-action') {
                Send-Frame @{ method = 'item/started'; params = @{ item = @{ type = 'commandExecution' } } }
                continue
            }
            if ($Mode -eq 'failed') {
                Send-Frame @{ method = 'turn/completed'; params = @{ turn = @{ status = 'failed'; error = @{ message = 'mock login expired' } } } }
                continue
            }
            $text = if ($Mode -eq 'json-object') { '{"arbitrary":{"values":[1,true,null]},"unicode":"测试"}' } elseif ($outputs.Count -gt 0) { 'Read result: ' + $outputs[-1].output[-1].text } else { '你好，Codex。' }
            Send-Frame @{ method = 'item/agentMessage/delta'; params = @{ delta = $text } }
            Send-Frame @{ method = 'thread/tokenUsage/updated'; params = @{ tokenUsage = @{ last = @{ inputTokens = 42; outputTokens = 8; cachedInputTokens = 5 } } } }
            Send-Frame @{ method = 'turn/completed'; params = @{ turn = @{ status = 'completed' } } }
            continue
        }
        default { throw "Unexpected method: $($request.method)" }
    }
    Send-Frame @{ id = $request.id; result = $result }
}
