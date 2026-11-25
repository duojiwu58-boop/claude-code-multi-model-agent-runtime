# Codex 路径能力扩展（2026-10-02）

本轮针对 [88 项接口审计](../service/audits/codex-official-api-20261002/report.txt) 中未支持的能力进行实现。测试后端为本机 Codex App Server 0.159.2 / gpt-6.1-sol，先使用隔离端口 18791 验收。随后按用户明确部署指令，于 2026-10-02 将验收版部署到 18787 生产服务，并配置服务级 PDF 渲染器路径；旧版本已备份。[部署记录](../service/deployments/codex-capabilities-20261002/deployment.json)。

## 已补齐的能力

| 能力 | 实现及实际边界 |
| --- | --- |
| JSON object | Responses `text.format` 和 Chat `response_format` 可用；支持任意键、嵌套对象。完成后验证有效 JSON 对象，无效时失败。此模式 SSE 缓冲至验证成功，不能宣称原生逐 token JSON 约束解码。JSON Schema 输出仍走原生 outputSchema。 |
| 请求级 reasoning effort | OpenAI 请求显式值优先于 profile 默认；Messages 原有策略不变。 |
| verbosity | low/medium/high 传给原生 thread config 的 model_verbosity；已捕获真实 Codex 向模型发出的 text.verbosity=low。 |
| 后台 Responses | background=true 立即返回 queued；查询、完成/失败、幂等取消；断开 HTTP 不结束后台生成。创建时 stream=true 可用 sequence_number / starting_after 恢复事件流。 |
| Files | multipart 上传、列表、信息、内容、删除、expires_after；Responses/Chat 支持 file_id；图片支持 input_image.file_id。支持 user_data/vision/assistants 本地用途，不创建 OpenAI 云端文件。 |
| PDF | inline PDF 与文件 ID 均可用。用 Poppler 渲染成图片交给同一 Codex 模型；真实模型正确读出了 PDF 校验词。依赖 pdftoppm（PATH 或 CLAUDE_BRIDGE_PDFTOPPM），不直接声称原生 PDF 支持。 |
| Chat 存储 | store=true 保存完成对象、输入和生成消息；支持列表、模型/metadata 过滤、读取、metadata 更新、删除及 messages 分页。 |
| Chat 多候选 | n=1..4，每个 choice 独立真实生成，支持 JSON/SSE 和汇总 usage；按顺序生成。含待回传工具结果的历史只允许 n=1，避免多个分支消费同一原生回调。 |
| custom 文本工具 | Responses custom_tool_call / custom_tool_call_output 及 SSE 可用；桥接成原生 function 的 input 字符串，保留原始文本及回调连续性。 |
| strict 工具参数 | strict=true 的 JSON Schema 在发送前编译、在交付调用前验证；无效参数中止，不交给客户端。使用本地 jsonschema 验证器，禁用网络/文件 schema 引用。不是原生 constrained decoding，因此仍可能生成失败。 |

后台任务最多 16 个活动任务、128 个内存记录，每个事件日志最多 8 MiB；终态内存记录保留 10 分钟。store=false 的后台响应仅短期保留于内存，不支持依赖磁盘记录的 input_items/previous_response_id。持久化响应可在重启后读取，未完成任务标记 failed；不虚构已丢失的执行线程或恢复事件日志。运行中的响应删除返回 409，先取消再删除。

文件最多 64 个、每个 8 MiB；UTF-8 文本及 PNG/JPEG/GIF/WebP 图片可用于输入，其他任意二进制不是可读文档。PDF 最多 16 页、渲染 30 秒、并发 2 个、输出图片合计 5 MiB；临时目录自动清理。响应 128 个、Chat 128 个、conversation 64 个；资源共用 32 MiB 原子 JSON 状态文件，展开输入最多 8 MiB。以上是明确的本地限额，不等于 OpenAI 云平台限额。

## 仍未提供等价支持的功能及原因

| 功能 | 原因、已有替代及继续支持的前提 |
| --- | --- |
| 精确 max_output_tokens/max_completion_tokens、thinking budget | 安装版 TurnStartParams 无对应硬预算字段；隐藏推理也消耗 token，截断可见文本无法限制实际生成/费用。已有本地字符限制和 stop；兼容模式报告警告，strict 模式生成前拒绝。需上游提供预算控制。 |
| temperature/top_p/top_k、seed、penalties、logit_bias、logprobs | 当前 App Server 无等价逐请求接口，也不返回 token 概率。不能由最终文本倒推出真实概率或伪造采样参数生效；需支持这些字段的原生模型 API。 |
| parallel_tool_calls=false 的完整规划约束 | 桥接已逐个交付回调，但不能阻止上游先规划多个调用。严格要求时拒绝；需原生 turn 参数支持禁用并行规划。 |
| strict 原生约束解码、custom grammar | 工具参数已本地验证；自由文本 custom 已适配。App Server dynamicTools 暴露 function/namespace，未提供 OpenAI grammar 解码控制。离线 grammar 检查只能发现错误，不能等价保证原生语法约束生成；完整支持需语法引擎及明确重试语义，或原生工具接口。 |
| 音频输入/输出、转写、Realtime | model/list 明确 gpt-6.1-sol 的输入模态为 text/image。向原生 inject_items 和 turn/start 分别发送合成语音，模型均未获得可听内容。协议存在 audio 类型不代表当前模型可用；需音频模型及执行接口，不能悄悄换模型并冒充 Codex。 |
| 托管 web_search/file_search/code_interpreter/computer/MCP 工具 | 当前 Codex 适配路径交付客户端 dynamicTools，没有实现 OpenAI 托管工具的执行资源、返回项、引用和计费契约。Codex 自带搜索/终端不等价于 OpenAI 对应工具。可用客户端 function/custom 工具执行业务，完整托管接口需额外后端和资源适配；不是只改 type 名称。 |
| 精确 responses/input_tokens | Host 不提供与实际模型、隐藏系统提示及工具开销一致的预计算 tokenizer。Messages count_tokens 可估算并明确标注；不能把估算当精确官方值。 |
| responses/compact、encrypted_content、context_management | 原生线程自动压缩可用，但不能导出/导入 OpenAI opaque encrypted compaction 项。普通摘要不能代替加密推理状态。需原生导出接口。 |
| truncation=auto 的官方语义 | Codex 自己管理上下文及压缩；桥接未实现 OpenAI 的精确旧项截断语义，尤其需维护工具调用/结果配对。当前明确拒绝此控制；不能把自动压缩称为等价截断。 |
| 缓存断点、prompt_cache_retention、服务等级履约 | 可报告原生给出的缓存 usage；serviceTier 可透传，但不能控制上游缓存保留或证明实际获得某一调度 SLA。缺少上游控制/确认字段。 |
| Embeddings、图片生成 | 当前路径只产生文本/工具输出，不提供向量头或图片生成输出。以文本数组/图像描述冒充向量/图片无效；需专用模型端点。 |
| Batch、fine-tuning、evals、云端 Files/vector stores | 本地队列和文件已实现，但没有 OpenAI 云端作业、训练执行、向量索引、Batch 优惠和 24 小时窗口契约。需要对应云 API 凭据和后端；Codex 登录不自动提供这些资源。 |
| file_url、Office/其他二进制文档 | 本轮支持文件 ID、inline UTF-8/图片/PDF；未实现远程 URL 获取和 Office 转换。继续支持需要有界下载、重定向校验及对应解析器，不能直接把未知二进制当文本。 |

这些限制是“当前后端不能等价提供”或“尚需独立后端实现”，并非断言任何系统都无法支持。配置具备对应能力的原生 OpenAI API profile 可走已有透传路径，但不会自动获取凭据、额度或把不支持的能力标为已验收。

## 证据和复验

- [实施计划及验证记录](../.planning/codex-capabilities-20261002/execplan.md)
- [真实接口验收脚本](../.planning/codex-capabilities-20261002/live_acceptance.py)、[48 项结果](../.planning/codex-capabilities-20261002/live-results.json)、[清理结果](../.planning/codex-capabilities-20261002/live-cleanup.json)
- 最终 Release：48/48 项接口验收、[2/2 项 strict JSON/SSE 实际调用](../.planning/codex-capabilities-20261002/strict-live-results.json)通过。离线 308 个桥接测试及 4 个 helper 测试通过，4 个登录测试默认忽略；最后的 Chat 消息修正另有定向回归。Clippy、格式及 Release 构建通过。[二进制 SHA256](../.planning/codex-capabilities-20261002/release-hashes.json)记录构建与生产的区别。
- [原生模型模态](../.planning/codex-capabilities-20261002/model_probe.json)、[原生控制字段](../.planning/codex-capabilities-20261002/native-controls.json)、[verbosity 出站验证脚本](../.planning/codex-capabilities-20261002/verbosity_wire_probe.py)
- 离线回归覆盖无效 JSON/strict 参数不交付、后台取消/重启、文件删除/二进制拒绝、Chat CRUD/n、多种工具续接。真实上游验收与 mock 回归分别记录，不以 HTTP 200 代替内容正确性。

官方依据：[Background](https://developers.openai.com/api/docs/guides/background)、[Structured Outputs](https://developers.openai.com/api/docs/guides/structured-outputs)、[Codex App Server](https://developers.openai.com/codex/app-server)、[Files](https://developers.openai.com/api/reference/typescript/resources/files/methods/create)、[Stored Chat](https://developers.openai.com/api/reference/typescript/resources/chat/subresources/completions/methods/list)。
