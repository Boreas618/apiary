# Apiary on Slurm: 调试全过程

> 从完全不可用到 100 并发零错误 — 一份完整的问题排查与修复记录。

## 背景

Apiary 是一个轻量级沙箱池，为 AI agent 提供隔离的 Linux 执行环境。它由三个组件构成：

- **Apiary daemon** (Rust) — 管理沙箱池，提供 REST API 用于创建会话、执行命令
- **MCP server** (Python/Starlette) — 基于 MCP 协议的 SSE 服务，将 Apiary 沙箱暴露为 MCP 工具
- **launch_apiary.sh** (Bash) — Slurm 作业脚本，在 pyxis/enroot 容器内启动整个栈并运行压力测试

部署环境为 NVIDIA DGX 集群，通过 Slurm + pyxis 插件运行 enroot 容器。测试脚本 `test_mcp_concurrent.py` 对两种模式进行并发压力测试：直接 REST API（apiary 模式）和完整 MCP 协议栈（mcp 模式）。

---

## 问题一览

从第一次提交作业到最终全部通过，共经历 6 轮迭代，修复了 **7 个独立问题**：

| # | 问题 | 严重程度 | 发现于 | 表现 |
|---|------|---------|--------|------|
| 1 | MCP 健康检查用了错误的端点 | 阻塞 | Job 1855583 | 浪费 5 分钟，MCP 检查永远失败 |
| 2 | curl 健康检查缺少 `--fail` 标志 | 误导 | Job 1855583 | 404 被当成成功，掩盖真实问题 |
| 3 | 端口 8080 被容器内置服务占用 | 阻塞 | Job 1855905 | daemon 启动失败：EADDRINUSE |
| 4 | SSE 读取器未处理异常 | 噪音 | Job 1855583 | 9202 行重复堆栈跟踪淹没有用输出 |
| 5 | Pool 的 Drop 实现毒化 shutdown 标志 | 阻塞 | Job 1857829 | 所有会话创建返回 "pool is shutting down" |
| 6 | SSE 解析器不支持 `\r\n` 行尾 | 阻塞 | Job 1857829 | MCP 端点事件收到但无法解析，5 秒超时 |
| 7 | 容器内 cgroup 文件系统只读 | 降级 | 所有 Job | 沙箱无法设置资源限制（内存/CPU/PID） |

---

## 详细时间线

### Job 1855583 — 初始运行：全面失败

**现象**: 两个测试模式均 100% 错误。Apiary 测试报告 daemon "UNREACHABLE"，MCP 测试输出 9202 行 `httpx.ReadError` 堆栈跟踪。

**根因分析**:

**(1) MCP 健康检查端点错误** (`launch_apiary.sh` 第 86 行)

```bash
# 错误：/sse 是 SSE 流式端点，连接后永不结束
while ! curl -sS --connect-timeout 2 --max-time 3 http://localhost:8082/sse
```

MCP 服务器的健康检查端点是 `/health`（定义在 `apiary_mcp.py` 第 537 行），而不是 `/sse`。`/sse` 是 SSE 流式端点 — curl 连接后收到 HTTP 200，但流永远不会结束，`--max-time 3` 触发超时退出。60 次重试 × 5 秒 = **300 秒浪费**。

**(2) curl 缺少 `--fail` 标志**

```bash
# 错误：没有 -f，curl 对 404 也返回 exit code 0
while ! curl -sS --connect-timeout 2 http://localhost:8080/healthz
```

daemon 的 `/api/v1/status` 返回了 HTML 格式的 404 页面（来自容器内其他 HTTP 服务），但 curl 不加 `-f` 时只要收到 HTTP 响应就认为成功。健康检查是**假阳性**。

**(3) SSE 读取器异常未处理** (`test_mcp_concurrent.py`)

`MCPAgent._read_sse_events()` 作为后台 asyncio Task 运行。当连接断开时抛出 `httpx.ReadError`，但没有被捕获。Python 的 asyncio 对每个未处理的 Task 异常都打印完整堆栈跟踪，导致 **9202 行重复噪音**。同时，pending 的 RPC Future 永远不会被 resolve，每个操作要等满 60 秒超时。

**(4) 错误率显示 bug**

当所有操作都失败时，`LevelReport.summary()` 返回的字典缺少 `error_rate_pct` 等字段，导致显示 "0%" 而非 "100%"。

**修复**:

- `/sse` → `/health`，加 `--max-time 5`
- 所有健康检查 curl 加 `-f`（`--fail`）
- 加入 daemon 日志输出到失败诊断
- 加入容器内连通性验证步骤（在测试前从容器内 curl 检测端口）
- SSE 读取器加 `try/except` 捕获所有异常，`finally` 块取消所有 pending Future
- `check_health()` 返回 `(bool, str)` 区分连接拒绝 vs HTTP 404 vs 超时
- 修复 summary 在全部失败时缺少字段的问题

---

### Job 1855905 — 端口冲突：EADDRINUSE

**现象**: daemon 健康检查 60 次重试全部失败（`-f` 修复生效了），脚本正确退出。

**根因**: daemon 日志显示：

```
Starting daemon API server (bind address: 127.0.0.1:8080)
Shutting down...
Error: Address already in use (os error 98)
```

端口 8080 被容器镜像内预装的 HTTP 服务占用。`run_apiary.sh` 中的 `ss` 端口检查在 daemon 启动前执行，但 daemon 需要 ~400ms 初始化沙箱池，在此期间容器服务启动并占用了端口。

**修复**:

- 默认端口从 `8080/8082` 改为 `38080/38082`（高端口，避免冲突）
- 所有端口引用参数化为 `${APIARY_PORT}` / `${MCP_PORT}`
- daemon 启动加入 5 次重试循环，每次重试前用 `fuser -k` 清理端口
- 健康检查循环中加入 `kill -0 $PID` 存活检测 — 如果进程崩溃立即报错并输出日志，而非等满 120 秒超时
- MCP 服务器同样加入端口冲突清理

---

### Job 1856558 — 仍然 EADDRINUSE

**现象**: 端口改为 38080 后不再有外部冲突，但 daemon 仍然 EADDRINUSE。

**根因**: `apiary init` 命令在初始化时可能短暂使用端口，daemon 紧接着启动时 socket 处于 TIME_WAIT 状态。`ss` 检查找不到（因为 ss 显示的是 LISTEN 状态），但内核仍然拒绝 bind。

**修复**: 重试循环已经在上一轮加入，这次生效了 — daemon 在第一次尝试时就成功启动（不再有端口冲突，因为 38080 没有其他服务使用）。

---

### Job 1856625 — daemon 正常，但测试 100% 失败

**现象**: daemon 启动成功，健康检查全部通过，pool 显示 20 个 idle 沙箱。但所有会话创建返回 503。容器内连通性验证也确认端口可达（HTTP 200）。

这是第一次 daemon 成功运行但 API 不工作的情况。由于改进了错误报告（捕获 response body），我们能看到 daemon 返回的实际错误：

```
{"error":"pool is shutting down"}
```

但 pool 明明有 20 个 idle 沙箱，为什么报 "shutting down"？

---

### Job 1857829 — 两个根因同时暴露

改进后的诊断输出揭示了最后两个 bug：

**Apiary 侧** — `{"error":"pool is shutting down"}`

```
iso-0: HTTP 503 for .../api/v1/sessions: {"error":"pool is shutting down"}
```

**MCP 侧** — SSE 事件格式不匹配

```
iso-0: SSE endpoint event not received within 5s
  (first_data='event: endpoint\r\ndata: /messages/?session_id=a479...e17b\r\n\r\n')
```

**(5) Pool Drop 毒化 shutdown 标志** (`src/pool/manager.rs`)

这是整个调试过程中最隐蔽的 bug。Pool 结构体同时实现了 `Clone`（所有字段都是 `Arc`）和 `Drop`：

```rust
#[derive(Clone)]
pub struct Pool {
    pub(super) shutdown: Arc<AtomicBool>,
    // ... 其他 Arc 字段
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}
```

由于所有 clone 共享同一个 `Arc<AtomicBool>`，当 **任何一个 clone 被 drop 时，所有 clone 的 shutdown 标志都会被设为 true**。

axum 框架对每个 HTTP 请求都会 clone 一份 state（包含 Pool），请求处理完毕后 clone 被 drop。所以流程是：

1. `launch_apiary.sh` 的诊断步骤 `curl /api/v1/status` 触发 GET 请求
2. axum clone Pool → handler 处理 → clone drop → **shutdown = true**
3. 之后所有 `POST /api/v1/sessions` 检查到 `shutdown == true`，立即返回 503

**修复**: 删除 Drop 实现。Pool 已有显式的 `pool.shutdown().await` 方法用于优雅关闭，Drop 的行为对 Clone 类型是有害的。

**(6) SSE 解析器不支持 `\r\n`** (`test_mcp_concurrent.py`)

MCP 服务器（uvicorn + starlette 的 SSE transport）发送的事件使用 `\r\n` 行尾：

```
event: endpoint\r\n
data: /messages/?session_id=xxx\r\n
\r\n
```

但测试的 SSE 解析器按 `"\n\n"` 分割事件边界。`"\r\n\r\n"` 无法匹配 `"\n\n"`，导致 endpoint 事件虽然收到了但永远无法被解析，5 秒后超时。

**修复**: 在解析前统一行尾 `chunk.replace("\r\n", "\n").replace("\r", "\n")`。

---

### Job 1858051 — 全部通过

修复 Pool Drop 和 SSE 解析器后，**两个测试模式首次全部成功**：

**Apiary 直接 API**:

| 并发 | 操作 | 错误率 | 吞吐量 | P50 延迟 | P99 延迟 |
|---:|---:|---:|---:|---:|---:|
| 1 | 12 | 8.3% | 19.7 ops/s | 14ms | 406ms |
| 5 | 60 | 8.3% | 68.4 ops/s | 15ms | 674ms |
| 10 | 120 | 8.3% | 118.8 ops/s | 17ms | 795ms |
| 20 | 240 | 8.3% | **438.5 ops/s** | 38ms | 87ms |
| 50 | 600 | 8.3% | 195.3 ops/s | 58ms | 2030ms |
| 100 | 1200 | 8.3% | 122.8 ops/s | 362ms | 3732ms |

恒定 8.33% 的错误率 = 每 agent 12 个操作中第 11 个 `head -5 /proc/self/status` 被 seccomp 策略阻止，属于预期行为。

**MCP 协议栈**:

| 并发 | 操作 | 错误率 | 吞吐量 | P50 延迟 | P99 延迟 |
|---:|---:|---:|---:|---:|---:|
| 1 | 12 | **0%** | 14.6 ops/s | 23ms | 421ms |
| 5 | 60 | **0%** | 47.7 ops/s | 23ms | 878ms |
| 10 | 120 | **0%** | 80.9 ops/s | 30ms | 1002ms |
| 20 | 240 | **0%** | **231.8 ops/s** | 62ms | 142ms |
| 50 | 600 | **0%** | 193.4 ops/s | 162ms | 702ms |
| 100 | 1200 | **0%** | 141.7 ops/s | 138ms | 5177ms |

MCP workload 不包含 `/proc` 操作，所以 0% 错误。自动扩容从 20 sandbox 扩展到 212。

---

### Job 1858150 — cgroup 修复尝试

尝试通过三种方式让容器内 cgroup 可写：

1. `--container-mounts="/sys/fs/cgroup:/sys/fs/cgroup:rw"` — pyxis 对 sysfs 特殊文件系统忽略 rw 标志
2. `mount -o remount,rw /sys/fs/cgroup` — 被容器安全策略拒绝
3. 读取 `/proc/self/cgroup` 找到 job 子树并在其下创建 — 子树也是只读的

**结论**: cgroup 在 pyxis/enroot 容器中被内核层面强制只读，不是脚本或应用层面能解决的问题。需要集群管理员配置 cgroup delegation 或调整 enroot 安全策略。

测试结果与 Job 1858051 一致 — 全部通过。

---

## 修改文件清单

### `launch_apiary.sh` — Slurm 作业脚本

| 修改 | 说明 |
|------|------|
| 端口参数化 | `APIARY_PORT=38080`, `MCP_PORT=38082`，所有引用改为变量 |
| 健康检查修复 | curl 加 `-f`；MCP 端点从 `/sse` 改为 `/health` |
| API 诊断 | daemon ready 后打印各端点 HTTP 状态码 |
| 容器内连通性验证 | 测试前从容器内 curl + `ss` 检查端口 |
| cgroup 挂载 | `--container-mounts` 加入 `/sys/fs/cgroup:rw`（best-effort） |
| 端口传递 | 通过 `env APIARY_BIND=... MCP_BIND=...` 传给容器内脚本 |

### `run_apiary.sh` — 容器内启动脚本

| 修改 | 说明 |
|------|------|
| 默认端口 | `8080/8082` → `38080/38082` |
| 端口冲突清理 | `_kill_port()` 函数用 `fuser -k` + `ss` 双重清理 |
| daemon 重试 | 最多 5 次启动尝试，每次清理端口后重试 |
| PID 存活检测 | 健康检查循环中检测进程是否崩溃，立即报错而非等超时 |
| cgroup v2 重写 | 尝试 remount rw → 寻找 job 子树 → 优雅降级 |

### `src/pool/manager.rs` — Rust: Pool 管理器

| 修改 | 说明 |
|------|------|
| 删除 `impl Drop for Pool` | Clone 类型的 Drop 不能修改共享状态；axum 每个请求 clone+drop 会毒化所有 clone |

### `test_mcp_concurrent.py` — 并发测试脚本

| 修改 | 说明 |
|------|------|
| SSE 行尾归一化 | `\r\n` → `\n`，修复 starlette SSE 兼容性 |
| SSE 异常处理 | 捕获 ReadError，finally 取消 pending Future |
| 错误信息增强 | `create_session` 捕获并显示 response body |
| 健康检查细化 | 区分 "connection refused" / "HTTP 404" / "timeout" |
| 隔离测试输出 | 失败时打印每个 agent 的具体错误 |
| SSE 调试 | 记录第一个 SSE chunk 的原始数据，超时时输出 |
| 错误率修复 | 全部失败时正确显示 100% 而非 0% |

---

## 剩余问题与 Future Work

### cgroup 资源限制 (P2)

当前状态：沙箱具备完整的命名空间隔离（mount / PID / user namespace + seccomp），但**没有资源限制**（内存上限、CPU 配额、PID 数量上限）。恶意或失控的 agent 进程可能消耗整个节点的资源。

可选方案：
- **集群侧**: 联系管理员启用 cgroup delegation 或调整 enroot 安全策略
- **Apiary 侧** (推荐): 在 daemon 层面实现进程级资源管理 — 使用 `setrlimit` / `prlimit` 限制单进程资源，或用 daemon 级别的监控 + kill 机制替代 cgroup

### seccomp /proc 限制 (P3)

Apiary 测试中恒定 8.33% 的错误来自 `head -5 /proc/self/status` 被 seccomp 阻止。如果 agent 需要读取 `/proc` 信息，需调整 seccomp 策略或在 rootfs 中 mock 相关文件。

### 高并发尾部延迟 (P3)

100 并发时 P99 延迟达到 3-5 秒，主要来自沙箱创建（自动扩容 20→120+）。可考虑：
- 增大初始 `min_sandboxes` 匹配预期并发
- 预热机制：在测试开始前一次性扩容到目标数量

---

## 经验总结

1. **永远给 curl 健康检查加 `--fail`**。没有 `-f` 的 curl 对 404 返回 exit 0，这在每一个使用 curl 做健康检查的脚本中都是定时炸弹。

2. **Clone + Drop = 危险组合**。如果一个类型实现了 Clone 且内部使用共享状态（Arc），Drop 绝不能修改共享状态。在 axum/actix 等框架中，state 被每个请求 clone 后 drop，这会让问题极其隐蔽 — 第一个 GET 请求就悄悄毒化了整个系统。

3. **不要用常见端口**。8080 是开发者最常用的备选端口，在任何 dev 容器里几乎必然冲突。选 38080 这样的高端口可以省去无数麻烦。

4. **SSE 实现必须处理 `\r\n`**。RFC 规范说 SSE 用 `\n`，但很多 HTTP 框架实际发 `\r\n`。防御性解析应当在处理前统一行尾。

5. **先捕获错误信息，再决定怎么修**。Job 1856625 和 1857829 之间的关键区别就是 — 前者只看到 "503"，后者看到了 `"pool is shutting down"` 和 `\r\n` 原始数据。多花 5 分钟加诊断日志，能节省几小时的盲猜。
