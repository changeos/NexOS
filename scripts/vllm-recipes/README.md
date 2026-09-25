# vLLM 启动配方库（实战沉淀）

GPU 服务器上经过实战验证的 vLLM 启动/管理脚本集合。每个脚本自含：uv 虚拟环境、
HF 镜像、启动（前台/后台）、停止、显存强制清理、状态/信息查看、实时日志、
IPMI 远程重启（可选）、基准压测（benchmark 脚本）。

## 配方清单

| 脚本 | 模型 | GPU | 关键参数 |
|---|---|---|---|
| `qwen3.6-27b-1gpu.sh` | Qwen/Qwen3.6-27B-FP8 | 1×GPU | 256K 上下文 · MTP 投机×2 · TP=1 |
| `qwen3.6-27b-8gpu.sh` | Qwen/Qwen3.6-27B-FP8 | 8×GPU | 同上 · TP=8 |
| `deepseek-v4-flash-4gpu.sh` | DeepSeek-V4-Flash-0731 | 4×H200 | 1M 上下文 · KV FP8 · DSpark 投机×7 · 专家并行 · API Key |
| `vllm-benchmark.sh` | 通用压测 | — | vllm bench serve 并发扫描（--p 起止） |

## 使用
```bash
scp scripts/vllm-recipes/qwen3.6-27b-1gpu.sh gpu-server:~/
ssh gpu-server 'chmod +x ~/qwen3.6-27b-1gpu.sh && ~/qwen3.6-27b-1gpu.sh'
```
脚本交互式菜单：启动（前台/后台）/停止/清理/信息/日志/IPMI 重启/更新 vLLM。

## 已知实战要点（脚本内已固化）
- python-ecdsa 类比不适用此处；vLLM 关键 env：`VLLM_WORKER_MULTIPROC_METHOD=spawn`、
  `VLLM_USE_RUST_FRONTEND`（前台 0/后台 1）
- DeepSeek-V4 需 `--tokenizer-mode deepseek_v4` + DeepGEMM（FP8 MoE 内核）
- 显存残留清理顺序：pkill → fuser /dev/nvidia* → /dev/shm → GPU reset
- HF 国内镜像：`HF_ENDPOINT=https://hf-mirror.com`
