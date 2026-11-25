# Messages / Responses / Chat → Codex 协议适配

这条路径由 Claude Code 执行 Read/Edit/Bash 等客户端工具，由已登录的本机 Codex App Server 调用模型。
当前验收对象为 Codex 0.159.2 / `gpt-6.1-sol`。目标是保留可表达的协议语义；两个产品的专有能力不能完全等价。

## 转换范围

| Claude 输入 / 输出 | Codex 转换 | 保真边界 |
| --- | --- | --- |
| 初始 system、历史 system/developer | baseInstructions；历史应用指令映射为 GPT developer message items | 保留内容与顺序，不降级为 user；system/developer 在 GPT 侧同属 developer 指令层；响应诊断标明映射 |
| clear_at: next_user_message | 后面有 user（含工具结果）时移除该条文本 | 默认与 never 保留；过期导致历史改变时重建 |
| 文本、URL/base64 图片、纯文本文档 | input_text / input_image；文档标注为不可信数据 | PDF/二进制文档、音频、引用块明确拒绝 |
| tool_use / tool_result | function_call / function_call_output；原生续接使用 item/tool/call RPC result | 保留名称映射、JSON 参数、调用 ID 配对、错误状态及文本/图片结果 |
| 客户端工具定义 | dynamicTools 的 inputSchema | strict=true 在桥接层校验参数后才交付；上游无 constrained decoding 保证 |
| 工具参数默认值 | 按 inputSchema.properties 的显式 default 补齐省略字段 | 发送 tool_use 与读取历史使用同一规则；不猜测未声明默认值，不覆盖显式值；解决 Claude Edit 回放补 replace_all=false 导致的误重建 |
| defer_loading / tool_addition / tool_removal | 按消息顺序计算本轮有效工具集合 | 支持客户端引用及内联定义；服务端工具/MCP toolset 不替代成客户端工具 |
| bash、text_editor、memory 客户端类型 | 展开为已支持类型的函数 schema | 实际动作仍由 Claude Code 执行 |
| tool_choice: auto/none/any/tool | 工具集合限制、要求调用的指令及回包校验 | 上游没有等价强制选择开关；不满足要求报错 |
| thinking | 请求公开 reasoning summary，输出无签名 thinking 块 | 摘要不是原始推理；不伪造 Anthropic 签名；历史 thinking 不当作文本重放 |
| output_config.effort | profile override > 最后系统 effort > 顶层 effort > disabled→none > profile default | 同名 effort 不等于同样 token 预算；模型不接受时返回上游错误 |
| JSON Schema 输出 | turn/start.outputSchema | 不改变 schema；不宣称与每种模型的 strict 保证相同 |
| agentMessage / reasoning summary 事件 | 独立 Anthropic content block / delta / stop | 进度与最终项不合并；重复 completed 不重复文本；摘要与文本分块 |
| token usage | total 累计量减上次已报告量 | input 扣除 cache read/write；output 包含 reasoning，reasoning_tokens 为额外统计字段 |
| 结束/失败/取消 | end_turn / tool_use 或错误事件、turn/interrupt | 不将失败伪装成功；只清理桥接拥有的线程 |
| 共享 Host 并发事件 | 按 threadId 过滤 | 忽略其他线程的广播、正文、用量、工具回调及结束事件 |

## 原生推理连续性

默认保留待返回结果的原生动态工具回调。下一条 HTTP 请求在历史前缀及模型/工具/指令/effort/schema
一致时，允许追加对应工具结果以及随后的 user/system/developer 消息。桥接器先通过 thread/inject_items
将后续消息排队，再回填原 RPC ID，继续同一个 thread 和 turn。已安装 0.159.2 的本地模拟上游验证了
模型实际收到的顺序为工具结果、附加指令、附加用户消息，而非 RPC 发送顺序。推理项仍由 Codex 持有。

GPT 推理模型使用 developer 承载应用级系统指令；0.159.2 会过滤排队中的原始 system 消息，因此
本适配器在发送边界显式转换角色，匹配客户端历史时仍保留原角色。不能保持原语义的历史编辑、压缩、
工具/模型/effort/schema 变化、工具结果之前插入消息、过期或重启会重建，绝不忽略差异来强行续接。

有稳定会话标识时，最终回答后保留空闲线程；下一用户轮次验证完整历史前缀，只注入新消息并启动同线程
的新 turn。接受 metadata.session_id、Claude metadata.user_id 中 JSON 格式的 session_id（连同设备和
账户范围）及旧式 _session_UUID 后缀。普通 user_id、相同提示词不能标识会话。标识仅以哈希存储/记录。
同一 Claude 会话的标题等辅助请求按应用基础指令分开管理，避免占用主对话线程。主对话的并发请求等待
前一请求释放归属；相同请求完成后重放缓存响应，等待计入总超时。

待回调和空闲线程合计最多 32 个，闲置上限 10 分钟；已完成响应缓存最多 64 项、32 MiB、10 分钟。
首次请求无稳定会话标识时不做跨请求合并，纯文本最终回答后也不保留可复用线程。
并行原生回调按 Claude HTTP 工具循环逐个交付，保留全部请求，但不承诺执行并行度等价。

`GET /admin/status` 的 codex_continuity 提供 counts、最多 64 条 recent、待回调/空闲线程数。模式包括
native_tool、native_tool_appended、native_user_turn、cached_reply，以及 settings_changed、history_changed、
session_changed、unsupported_append_order、new_session_or_expired、no_retained_state、continuation_disabled。
recent 包含线程 ID、会话/基础指令范围哈希、工具数、耗时和用量，不包含正文。重建时日志只显示首个不同项
的位置、角色、内容块长度及工具参数键/哈希。统计针对已完成 HTTP 响应，cached_reply 的用量不重复计入。
Claude Bash 有时会在执行后回放时去掉 `cd ... &&` 前缀；无法仅凭命令文字证明其持久工作目录未改变，
因此这一类实际参数改写仍报告 history_changed 并重建。声明的 schema 默认值补全不属于这种改写。

## 显式差异

- `max_tokens`、temperature/top_p/top_k：App Server 没有等价逐请求控制；默认兼容模式用响应头报告，`bridge_controls.mode=strict` 在生成前拒绝。
- `service_tier` 原生传递到 turn/start.serviceTier；reasoning.summary 的 auto/concise/detailed 原生传递到 summary，是否接受由上游决定。Host 未回传实际层级，所以 Responses 的 service_tier 为 null，bridge_details 中保留请求值。
- `thinking.budget_tokens`：不能换算为相同 token 预算；使用 effort。
- `cache_control`：忽略 Anthropic 缓存断点及 TTL，由 Codex 自己缓存，不伪造缓存命中。
- context_management：支持空 edits、clear_thinking_20251015 的 keep=all（不删除内容），并提示原生状态续接的范围；主动删除 thinking/工具结果的策略明确拒绝。
- stop_sequences（Chat 的 stop）在可见文本上本地执行，保留跨 chunk 的停止串前缀，不截断工具 JSON。`bridge_controls.max_output_chars` 按 Unicode 字符计数，与 token/推理费用上限不同；与 JSON Schema 同时使用会拒绝。触发本地终止会中断 turn 并丢弃原生续接；Messages 的 usage.bridge_usage_complete=false，OpenAI usage=null，不能按零消耗计费。
- container、mcp_servers：拒绝不支持的控制语义。
- 模型重路由：明确报错，不把其他模型的结果标成请求模型。
- 缓存未命中后的历史重建不保留隐藏推理；不能据普通回答成功声称完全无损。

诊断通过现有桥接响应头输出。续接/重建在服务日志中记录，不记录用户正文。

## OpenAI HTTP 入口

所有以下接口使用同一个本地 Bearer token。活动 codex profile 的模型名必须与请求相同，不会误走 Gemini。

| 路径 | 当前 Codex 行为 |
| --- | --- |
| POST /v1/responses | 文本、图像、file_id/inline 文件、PDF、function/custom 文本工具、JSON Schema/JSON object、reasoning summary、background；JSON 或 SSE |
| POST /v1/chat/completions | messages、function tool_calls、response_format、stop、store、n=1..4；JSON 或 chunk / finish_reason / [DONE] 流；可选最终 usage chunk |
| GET / DELETE /v1/responses/{id} | 查询/删除桥接保存的响应 |
| POST /v1/responses/{id}/cancel | 幂等取消后台任务；仅中断任务自己的 Codex turn |
| GET /v1/responses/{id}?stream=true&starting_after=N | 为创建时 stream=true 的后台任务恢复事件流 |
| POST / GET /v1/files | 上传 multipart 文件 / 分页列出本地文件 |
| GET / DELETE /v1/files/{id}；GET /v1/files/{id}/content | 文件信息、删除、原始字节下载 |
| GET /v1/chat/completions；GET / POST / DELETE /v1/chat/completions/{id} | 列出、读取、更新 metadata、删除 store=true 的 Chat |
| GET /v1/chat/completions/{id}/messages | 查询保存的输入消息 |
| GET /v1/responses/{id}/input_items | 完整输入，支持 limit、order、after 分页 |
| POST /v1/conversations | 新建本地对话，可给 items 和 metadata |
| GET / POST / DELETE /v1/conversations/{id} | 查询、更新 metadata、删除 |
| GET / POST /v1/conversations/{id}/items | 分页查询、追加输入项 |
| GET / DELETE /v1/conversations/{id}/items/{item} | 查询/删除单项 |
| POST /v1/responses/input_tokens | 501：Host 不提供精确预计算 tokenizer；Messages count_tokens 继续明确标注估算 |
| POST /v1/responses/compact | 501：Host 不能导出 OpenAI 加密 compaction 项；原生线程自动压缩由 Codex 管理 |

Responses 用 previous_response_id 或 conversation 延续上下文，两者不能同时设置。前一响应的 instructions 不自动继承；需要原生线程连续性时，应继续提供相同 instructions 和工具配置。上下文或配置变化触发可见历史重建，隐藏推理不随磁盘记录恢复。工具回传图片按原始顺序注入同线程补充用户媒体，标明工具来源；回调保留文本、错误和调用 ID，不重复执行工具。

Responses 默认 store=true，将输入输出持久化到桥接状态文件旁的 `*.openai.json`，与原有仅在内存的原生线程缓存不同。最多 128 个 response、64 个 conversation、128 个 stored Chat、64 个文件，共用 32 MiB；单次展开输入最多 8 MiB。原子替换写入，达到上限返回错误并保留原文件；普通记录保存至显式删除，无额外应用层加密。store=false 不持久化响应，但指定 conversation 时仍更新该对话。Chat 显式 store=true 才保存。对话更新在生成期间返回 409，防止并发覆盖；响应上下文按 provider/model 校验。

示例（请求 `/v1/responses`）：

```json
{"model":"gpt-6.1-sol","input":"只回答 OK","stream":true,"store":false,"bridge_controls":{"mode":"strict"}}
```

max_output_tokens / max_completion_tokens 映射到相同兼容诊断，严格模式拒绝。OpenAI 入口显式 reasoning.effort/reasoning_effort 优先于 profile 默认；Messages 入口的 profile/GUI 策略不变。text.verbosity 经原生 thread config 传递。parallel_tool_calls=false 只能逐个交付回调，不能约束上游规划，兼容模式提示、严格模式拒绝。strict=true 在交付工具调用前执行本地 JSON Schema 校验，失败中止请求；不保证上游 constrained decoding，不读取网络或磁盘 $ref。不支持的请求字段和内容明确报错。

活动 openai-responses profile 的 Responses、资源、精确计数及 compact 接口按原 JSON 转发到原生 HTTP endpoint；活动 openai profile 的 Chat 接口按原 JSON 转发；文件转发保留 multipart Content-Type。需要音频、托管工具、精确预算或加密 compaction 时，须显式配置具有相应能力和凭据的原生 API 提供商。Codex 登录不会自动提供该 API 额度或凭据。原生 HTTP 转发有 mock 验证，未用外部 API 凭据做真实托管功能验收。

本轮新增能力、实际限额、验收证据及仍未实现功能的逐项原因见 [Codex 能力扩展结果](CODEX_CAPABILITIES.md)。

## 2026-10-02 修复验收

详见 [实施记录](../.planning/codex-fixes-20261002/execplan.md)。独立 18790 实例的 35 项真实 HTTP 检查通过，包括两种 JSON/SSE、结构化输出、文本文件、本地停止条件、资源 CRUD、同线程两轮多图工具回传和能力错误。多图流程五个请求使用同一 thread，模式依次为新线程、native_tool_appended、native_user_turn、native_user_turn、native_tool_appended。生产上线及回滚备份单独记录在部署目录。

## 历史验证记录

普通 `cargo test --locked` 使用 mock，不消耗模型额度。Codex 测试覆盖角色和 clear_at、工具及错误/图片结果、
动态工具、严格历史匹配、并发重试、流取消、多个排队回调、usage 差分和 SSE 去重。

显式登录测试 `codex_live_bridge_two_native_tool_cycles` 已在 `gpt-6.1-sol` 通过：第二个文件名只能从
第一次工具结果获得，两次工具调用断言 threadId/turnId 相同，最终答案核对第二个结果的校验词。
这验证的是桥接器到真实上游；原生 Claude Code 面板验收另外记录在部署目录，不混同两种证据。

2026-10-02 本机部署验收：完整离线回归 285 项通过（3 个登录测试默认忽略），Clippy、格式及 Release
构建通过。共享 Host 的 3 个真实并发 HTTP 请求全部返回 200，校验词各自正确。通过原生面板连续
读取 Cargo.toml 与 src/README.md，26 秒内完整返回正确包名、版本和模块职责，无 400/502。
证据：[并发结果](../service/deployments/codex-protocol-20261002/concurrency.json)、
[原生面板结果](../service/deployments/codex-protocol-20261002/panel-verification.json)。

本次真实 Claude Code 在工具结果之外附带一条额外消息：设置和已有历史一致，但内容项数增加，
因此两次续接走了可见历史重建。面板测试证明端到端可用，不证明这个客户端流程保留了隐藏推理。
保留原生推理的证据仅适用于上面的精确工具结果续接测试。自动计时淘汰与真实图片理解未做等待式/
视觉端到端验收；图片转换及取消逻辑有离线覆盖。

以上为首次部署记录。后续连续性改进的验收见
[连续性实施计划](../.planning/codex-continuity-20261002/execplan.md)。新增真实上游测试
`codex_live_appended_context_and_next_user_turn_keep_thread` 已验证附加 user/system 指令、同 turn 工具循环
及同 thread 下一用户轮次。真实原生面板完成 Read/Edit/Bash 的失败→修正→通过流程，独立复跑 5 项测试通过。
本轮完整离线回归 296 项通过（4 个登录测试默认忽略），Clippy、格式及 Release 构建通过。
本轮使用独立 18789 测试实例，验收后已关闭并移除临时客户端配置，未替换生产服务。

相同初始代码和提示词的最终面板对照如下，模型为 gpt-6.1-sol / high：

| 模式 | 主任务请求 | 原生续接 | 使用线程 | 面板总耗时 | 最终测试 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 开启续接 | 12 | 9 / 11 次后续请求 | 3 | 79.7 秒 | 5/5 |
| 关闭续接 | 12 | 0 | 12 | 117.3 秒 | 5/5 |

开启续接时的 2 次重建均因 Claude 改写 Bash 命令、去掉 `cd ... &&` 前缀，保留严格历史校验。
[最终对照指标](../.planning/codex-continuity-20261002/comparison.json)附原始面板和逐请求指标引用。
这只是一次配对样本，不能推广为稳定性能增益。部分工具边界尚未上报最终用量，尤其是重放模式关闭
待回调 turn 时；汇总用量仅供诊断，不可据此计算实际账单或 token 节省比例。

## 官方依据

- [OpenAI App Server](https://learn.chatgpt.com/docs/app-server)：动态工具回调、线程/turn、事件、用量；并核对本机 0.159.2 导出的 schema。
- [OpenAI Reasoning](https://developers.openai.com/api/docs/guides/reasoning)：工具循环中的 reasoning 连续性、摘要与原生 reasoning 的区别。
- [Reasoning best practices](https://developers.openai.com/api/docs/guides/reasoning-best-practices)：GPT 推理模型以 developer 承载应用级系统指令。
- [OpenAI Responses 迁移](https://developers.openai.com/api/docs/guides/migrate-to-responses)：消息及函数调用 items。
- [Claude 中途系统消息与工具变化](https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages)：角色、clear_at、工具增删。
- [Claude SSE](https://platform.claude.com/docs/en/build-with-claude/streaming)：内容块与流式终态。
- [Claude 工具结果](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls)：调用 ID 与结果契约。
- [Claude effort](https://platform.claude.com/docs/en/build-with-claude/effort)：顶层及中途 effort。
- [Claude 上下文编辑](https://platform.claude.com/docs/en/build-with-claude/context-editing)：keep=all 与主动删除策略的区别。
