# ============================================================================
# verify-gui-probe-state.ps1
# 静态回归检查（无需编译器）：
#   模型连通性测试（TMainForm.TestProfileAsync）不得进入全局 FBusy /
#   SetBusy / Screen.Cursor，而应使用独立在途状态 FTestInFlight，且只
#   禁用"测试"按钮；恢复路径（SetBusy / SetBridgeState）不得在探测
#   在途时重新启用"测试"按钮（防止重复探测）。
#   探测文案本地化（中文"运行记录"）：成功描述必须按 transport /
#   upstream_status 渲染，失败阶段与桥接器稳定文案必须经中文映射，
#   映射表不得删除或回退为英文。
#
# 用法： powershell -NoProfile -ExecutionPolicy Bypass -File verify-gui-probe-state.ps1
# 退出码：0 = 通过；1 = 检出回归。
# ============================================================================
param(
    [string]$PasPath = "$PSScriptRoot\BridgeManagerMain.pas"
)

$ErrorActionPreference = 'Stop'
$failures = New-Object System.Collections.Generic.List[string]

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { $failures.Add($Message) }
}

if (-not (Test-Path -LiteralPath $PasPath)) {
    Write-Output ("FAIL: file not found: " + $PasPath)
    exit 1
}

$text = [System.IO.File]::ReadAllText($PasPath)
$lines = $text -split "`r?`n"

function Find-Line {
    param([string]$Pattern, [int]$From = 0)
    for ($i = $From; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match $Pattern) { return $i }
    }
    return -1
}

# ---------------------------------------------------------------------------
# 1. FTestInFlight 字段必须声明（独立于 FBusy 的在途状态）
# ---------------------------------------------------------------------------
Assert-True ($text -match 'FTestInFlight\s*:\s*Boolean') `
    'missing: private field FTestInFlight: Boolean'

# ---------------------------------------------------------------------------
# 2. 全局十字光标只允许出现在 SetBusy（探测路径接近）
# ---------------------------------------------------------------------------
Assert-True ((($text | Select-String -Pattern 'Screen\.Cursor\s*:=\s*crHourGlass' -AllMatches).Matches.Count) -eq 1) `
    'unexpected: Screen.Cursor := crHourGlass must appear exactly once (only in SetBusy)'

# ---------------------------------------------------------------------------
# 3. TestProfileAsync 过程切片检查
# ---------------------------------------------------------------------------
$testStart = Find-Line '^\s*procedure\s+TMainForm\.TestProfileAsync'
Assert-True ($testStart -ge 0) 'missing: procedure TMainForm.TestProfileAsync'

$testEnd = -1
if ($testStart -ge 0) {
    for ($i = $testStart + 1; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match '^\s*end\)\.Start;\s*$') {
            $j = $i + 1
            while ($j -lt $lines.Count -and $lines[$j].Trim() -eq '') { $j++ }
            if ($j -lt $lines.Count -and $lines[$j] -match '^\s*end;\s*$') { $testEnd = $j; break }
        }
    }
}
Assert-True ($testEnd -ge 0) 'missing: could not delimit TestProfileAsync body'

if ($testStart -ge 0 -and $testEnd -ge 0) {
    $body = ($lines[$testStart..$testEnd] -join "`n")

    Assert-True ($body -match 'FTestInFlight\s*:=\s*True') `
        'missing: TestProfileAsync must set FTestInFlight := True on start'
    Assert-True ($body -match 'FTestInFlight\s*:=\s*False') `
        'missing: TestProfileAsync must clear FTestInFlight := False on completion'
    Assert-True ($body -match 'FTestButton\.Enabled\s*:=\s*False') `
        'missing: TestProfileAsync must disable the Test button only'
    Assert-True ($body -notmatch '\bSetBusy\(') `
        'regression: TestProfileAsync must not call SetBusy (global busy state)'
    Assert-True ($body -notmatch 'Screen\.Cursor') `
        'regression: TestProfileAsync must not use global cursor'
    Assert-True ($body -match 'Inc\(FWorkerCount\)') `
        'missing: TestProfileAsync must keep FWorkerCount balancing'
    Assert-True ($body -match 'FClosing\s+or\s+FBusy\s+or\s+FTestInFlight') `
        'missing: TestProfileAsync guard must reject duplicate probes (FTestInFlight)'
}

# ---------------------------------------------------------------------------
# 4. 恢复路径不得绕过在途状态
# ---------------------------------------------------------------------------
Assert-True ($text -match 'FTestButton\.Enabled\s*:=\s*\(FStatusLabel\.Tag = 1\)\s+and\s+not\s+FTestInFlight\s*;') `
    'regression: SetBusy(False) must not re-enable Test while a probe is in flight'
Assert-True ($text -match 'FTestButton\.Enabled\s*:=\s*not\s+FTestInFlight\s*;') `
    'regression: SetBridgeState(online) must not re-enable Test while a probe is in flight'

# ---------------------------------------------------------------------------
# 5. 中文"运行记录"本地化不得回退为后端英文文案：
#    成功描述必须按 transport / upstream_status 渲染；失败阶段与桥接器
#    稳定文案必须经中文映射表，且映射表不得被删除或改回英文。
# ---------------------------------------------------------------------------
Assert-True ($text -match 'function\s+TMainForm\.ProbeStageName') `
    'missing: probe stage → Chinese label mapping (ProbeStageName)'
Assert-True ($text -match 'function\s+TMainForm\.ProbeTransportName') `
    'missing: probe transport → protocol name mapping (ProbeTransportName)'
Assert-True ($text -match 'function\s+TMainForm\.LocalizeProbeMessage') `
    'missing: stable bridge-owned probe message translation (LocalizeProbeMessage)'
Assert-True ($text -match 'function\s+TMainForm\.ProbeSuccessSummary') `
    'missing: Chinese success summary renderer (ProbeSuccessSummary)'
Assert-True ($text -match 'function\s+TMainForm\.ProbeNetworkSummary') `
    'missing: staged probe network summary renderer (ProbeNetworkSummary)'

# 阶段标签映射（简洁中文，不得删减或改回英文标签；此处为精确匹配）
Assert-True $text.Contains("AStage = 'resolve' then Result := '配置解析'") `
    'missing: stage resolve must map to 配置解析'
Assert-True $text.Contains("AStage = 'auth' then Result := '鉴权'") `
    'missing: stage auth must map to 鉴权'
Assert-True $text.Contains("AStage = 'upstream' then Result := '上游'") `
    'missing: stage upstream must map to 上游'
Assert-True $text.Contains("AStage = 'response' then Result := '响应'") `
    'missing: stage response must map to 响应'
Assert-True $text.Contains("AStage = 'timeout' then Result := '超时'") `
    'missing: stage timeout must map to 超时'
Assert-True $text.Contains("AStage = 'bridge' then Result := '桥接器'") `
    'missing: stage bridge must map to 桥接器'

# 传输协议名保留（Anthropic / Gemini Interactions / OpenAI Chat/Responses）
Assert-True $text.Contains("ATransport = 'anthropic' then Result := 'Anthropic'") `
    'missing: transport anthropic must map to Anthropic'
Assert-True $text.Contains("ATransport = 'gemini-interactions' then Result := 'Gemini Interactions'") `
    'missing: transport gemini-interactions must map to Gemini Interactions'
Assert-True $text.Contains("ATransport = 'openai-chat' then Result := 'OpenAI Chat'") `
    'missing: transport openai-chat must map to OpenAI Chat'
Assert-True $text.Contains("ATransport = 'openai-responses' then Result := 'OpenAI Responses'") `
    'missing: transport openai-responses must map to OpenAI Responses'

# 桥接器稳定文案的翻译必须存在（保留技术标识与动态片段）
Assert-True $text.Contains('无法读取提供程序路由状态') `
    'missing: must translate "Cannot read provider routing state"'
Assert-True $text.Contains('未找到模型配置：') `
    'missing: must translate "Unknown provider profile ..."'
Assert-True $text.Contains('探测超时（上限 ') `
    'missing: must translate "Profile probe timed out after ..."'
Assert-True $text.Contains('本地 Gemini 需要 GEMINI_API_KEY 或 GEMINI_BRIDGE_API_KEY_PROFILE') `
    'missing: must translate "LocalGemini requires ..."'
Assert-True $text.Contains('无法读取探测响应：') `
    'missing: must translate "Cannot read probe response: ..."'
Assert-True $text.Contains('探测响应不是有效的 Anthropic message') `
    'missing: must translate "invalid Anthropic message response"'

# TestProfileAsync 必须读取 transport/upstream_status 并经中文映射渲染
if ($testStart -ge 0 -and $testEnd -ge 0) {
    Assert-True $body.Contains(" LTransport := JsonText(LJson, 'transport');") `
        'missing: TestProfileAsync must read transport from the probe result'
    Assert-True $body.Contains(" LUpstreamStatus := JsonText(LJson, 'upstream_status');") `
        'missing: TestProfileAsync must read upstream_status from the probe result'
    Assert-True $body.Contains('ProbeSuccessSummary(LTransport, LUpstreamStatus, LMessage)') `
        'regression: success log must render the Chinese summary, not the raw English message'
    Assert-True $body.Contains('ProbeStageName(LStage)') `
        'regression: failure stage must be translated before display/log'
    Assert-True $body.Contains('LocalizeProbeMessage(LMessage)') `
        'regression: failure message must pass through the Chinese localization map'
    foreach ($field in @('proxy_mode', 'proxy_url', 'response_headers_ms',
            'first_byte_ms', 'response_body_ms', 'total_ms')) {
        Assert-True $body.Contains("JsonText(LNetwork, '$field')") `
            ("missing: TestProfileAsync must read network field " + $field)
    }
    Assert-True $body.Contains('ProbeNetworkSummary(') `
        'regression: probe logs must render staged network diagnostics'
    Assert-True $text.Contains("AddPart('连接/响应头', AResponseHeadersMs + ' ms')") `
        'missing: response-header phase must be labelled in Chinese'
    Assert-True $text.Contains("AddPart('首包', AFirstByteMs + ' ms')") `
        'missing: first-byte phase must be labelled in Chinese'
    Assert-True $text.Contains("AddPart('响应体完成', AResponseBodyMs + ' ms')") `
        'missing: response-body phase must be labelled in Chinese'
    Assert-True $text.Contains("AddPart('总耗时', ATotalMs + ' ms')") `
        'missing: total phase must be labelled in Chinese'
}

# ---------------------------------------------------------------------------
if ($failures.Count -eq 0) {
    Write-Output 'PASS: probe uses FTestInFlight, blocks only Test button, no global busy/cursor.'
    Write-Output 'PASS: probe log renders Chinese success/failure descriptions via stable mappings.'
    Write-Output 'PASS: probe log renders proxy mode and staged network timings.'
    exit 0
} else {
    Write-Output ('FAIL: ' + $failures.Count + ' check(s) failed:')
    foreach ($f in $failures) { Write-Output ('  - ' + $f) }
    exit 1
}
