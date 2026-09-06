#!/bin/bash

# DeepSeek-V4-Flash-0731 交互式管理脚本（使用 uv 虚拟环境）
# 适配非 root 用户，支持国内镜像加速，支持 1M 上下文（4×H200）

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# ==================== 配置区域 ====================
# 模型配置
MODEL_NAME="deepseek-ai/DeepSeek-V4-Flash-0731"
SERVED_MODEL_NAME="DeepSeek-V4-Flash"
SERVER_IP="0.0.0.0"
PORT="8123"                         # 端口设为 8123
BASE_URL="http://${SERVER_IP}:${PORT}"
LOG_FILE="/tmp/deepseek_v4_vllm.log"
API_KEY="<your-api-key>"                 # API Key

# GPU 配置（4 卡）
GPU_IDS="0,1,2,3"
TENSOR_PARALLEL_SIZE=4

# uv 虚拟环境路径
UV_VENV_PATH="$HOME/vllm"

# IPMI 配置（可选）
IPMI_HOST="10.10.3.159"
IPMI_USER="admin"
IPMI_PASS="admin"

# vLLM 启动参数（移除 max-num-seqs 限制）
GPU_MEMORY_UTIL=0.85
DTYPE="auto"
KV_CACHE_DTYPE="fp8"
BLOCK_SIZE=256
MAX_MODEL_LEN=1048576               # 1M 上下文
MAX_NUM_BATCHED_TOKENS=8192         # 每批最大 token 数
# MAX_NUM_SEQS 已移除，vLLM 自动管理

# ==================== 网络镜像配置 ====================
USE_HF_MIRROR=true
HF_MIRROR_URL="https://hf-mirror.com"
USE_MODELSCOPE=false
# =====================================================
# ================================================

# 获取本机活动 IPv4 地址
get_local_ip() {
    local ip=$(hostname -I 2>/dev/null | awk '{print $1}')
    if [ -n "$ip" ] && [ "$ip" != "127.0.0.1" ]; then
        echo "$ip"
        return
    fi
    ip=$(ip -4 addr show scope global 2>/dev/null | grep inet | awk '{print $2}' | cut -d/ -f1 | head -1)
    if [ -n "$ip" ]; then
        echo "$ip"
        return
    fi
    echo "0.0.0.0"
}

check_service_by_process() {
    if pgrep -f "VLLM::Worker" > /dev/null || pgrep -f "vllm.*${MODEL_NAME}" > /dev/null; then
        return 0
    else
        return 1
    fi
}

check_service_http() {
    if curl -s -f -o /dev/null "${BASE_URL}/health" 2>/dev/null; then
        return 0
    else
        return 1
    fi
}

get_service_status() {
    if check_service_http; then
        echo -e "${GREEN}● 运行中 (HTTP)${NC}"
        PID=$(pgrep -f "VLLM::Worker" | head -1)
        [ -n "$PID" ] && echo -e "  Worker PID: $PID"
    elif check_service_by_process; then
        echo -e "${YELLOW}● 进程运行中 (VLLM::Worker)${NC}"
        PID=$(pgrep -f "VLLM::Worker" | head -1)
        echo -e "  Worker PID: $PID"
    else
        GPU_MEM=$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits 2>/dev/null | head -1)
        if [ -n "$GPU_MEM" ] && [ "$GPU_MEM" -gt 1000 ]; then
            echo -e "${RED}● 未运行 (但有显存残留: ${GPU_MEM}MiB)${NC}"
        else
            echo -e "${RED}● 未运行${NC}"
        fi
    fi
}

cleanup_gpu_memory() {
    echo "清理 GPU 显存残留..."
    systemctl stop nvidia-persistenced 2>/dev/null
    systemctl stop nvidia-fabricmanager 2>/dev/null
    nvidia-smi -pm 0 2>/dev/null
    pkill -9 -f "VLLM::Worker" 2>/dev/null
    pkill -9 -f "vllm.*DeepSeek" 2>/dev/null
    pkill -9 -f "python.*vllm" 2>/dev/null
    fuser -k /dev/nvidia* 2>/dev/null
    sleep 2
    for i in {0..7}; do
        nvidia-smi --gpu-reset -i $i 2>/dev/null
    done
    rm -rf /dev/shm/* 2>/dev/null
    rm -rf /tmp/torch_cache_* 2>/dev/null
    rm -rf /tmp/vllm_* 2>/dev/null
    sleep 2
    GPU_MEM=$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits 2>/dev/null | head -1)
    if [ -n "$GPU_MEM" ] && [ "$GPU_MEM" -gt 1000 ]; then
        echo -e "${YELLOW}⚠ 显存仍有残留 (${GPU_MEM}MiB)，建议重启服务器${NC}"
    else
        echo -e "${GREEN}✓ 显存已清理${NC}"
    fi
}

start_service_foreground() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    启动 DeepSeek-V4-Flash-0731 服务${NC}"
    echo -e "${BLUE}    前台模式（Ctrl+C 停止服务）${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""
    if check_service_by_process; then
        echo -e "${YELLOW}检测到残留进程，正在清理...${NC}"
        pkill -9 -f "VLLM::Worker" 2>/dev/null
        pkill -9 -f "vllm" 2>/dev/null
        sleep 2
    fi
    echo "正在启动服务，日志将实时显示..."
    echo -e "${YELLOW}提示：按 Ctrl+C 可停止服务并返回菜单${NC}"
    echo ""
    read -p "按 Enter 继续..."

    echo -e "${YELLOW}正在激活虚拟环境...${NC}"
    source ${UV_VENV_PATH}/bin/activate

    if [ "$USE_HF_MIRROR" = true ]; then
        export HF_ENDPOINT="${HF_MIRROR_URL}"
        echo -e "${GREEN}✓ 使用 Hugging Face 镜像: ${HF_ENDPOINT}${NC}"
    fi
    if [ "$USE_MODELSCOPE" = true ]; then
        export VLLM_USE_MODELSCOPE=True
        echo -e "${GREEN}✓ 启用 ModelScope 镜像${NC}"
    fi

    # 必须的环境变量
    export VLLM_WORKER_MULTIPROC_METHOD=spawn
    export CUDA_VISIBLE_DEVICES=${GPU_IDS}
    export VLLM_USE_RUST_FRONTEND=1
    export VLLM_API_KEY=${API_KEY}

    echo -e "${YELLOW}正在启动 vLLM 服务，请耐心等待模型加载...${NC}"
    vllm serve ${MODEL_NAME} \
      --served-model-name ${SERVED_MODEL_NAME} \
      --trust-remote-code \
      --port ${PORT} \
      --host ${SERVER_IP} \
      --tensor-parallel-size ${TENSOR_PARALLEL_SIZE} \
      --dtype ${DTYPE} \
      --kv-cache-dtype ${KV_CACHE_DTYPE} \
      --block-size ${BLOCK_SIZE} \
      --gpu-memory-utilization ${GPU_MEMORY_UTIL} \
      --max-model-len ${MAX_MODEL_LEN} \
      --max-num-batched-tokens ${MAX_NUM_BATCHED_TOKENS} \
      --enforce-eager \
      --enable-expert-parallel \
      --tokenizer-mode deepseek_v4 \
      --tool-call-parser deepseek_v4 \
      --enable-auto-tool-choice \
      --reasoning-parser deepseek_v4 \
      --speculative-config '{"method":"dspark","num_speculative_tokens":7,"draft_sample_method":"probabilistic"}' \
      --api-key ${API_KEY}
    echo ""
    echo -e "${YELLOW}服务已停止${NC}"
    sleep 2
}

start_service_background() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    启动 DeepSeek-V4-Flash-0731 服务${NC}"
    echo -e "${BLUE}    后台模式${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""
    if check_service_by_process; then
        echo -e "${YELLOW}检测到残留进程，正在清理...${NC}"
        pkill -9 -f "VLLM::Worker" 2>/dev/null
        pkill -9 -f "vllm" 2>/dev/null
        sleep 2
    fi
    GPU_MEM=$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits 2>/dev/null | head -1)
    if [ -n "$GPU_MEM" ] && [ "$GPU_MEM" -gt 10000 ]; then
        echo -e "${YELLOW}⚠ 检测到显存残留 (${GPU_MEM}MiB)${NC}"
        echo -n "是否清理？[y/N]: "
        read -r answer
        if [[ "$answer" =~ ^[Yy]$ ]]; then
            cleanup_gpu_memory
        fi
    fi
    echo "正在启动服务..."
    echo ""

    echo -e "${YELLOW}正在激活虚拟环境...${NC}"
    source ${UV_VENV_PATH}/bin/activate

    if [ "$USE_HF_MIRROR" = true ]; then
        export HF_ENDPOINT="${HF_MIRROR_URL}"
        echo -e "${GREEN}✓ 使用 Hugging Face 镜像: ${HF_ENDPOINT}${NC}"
    fi
    if [ "$USE_MODELSCOPE" = true ]; then
        export VLLM_USE_MODELSCOPE=True
        echo -e "${GREEN}✓ 启用 ModelScope 镜像${NC}"
    fi

    export VLLM_WORKER_MULTIPROC_METHOD=spawn
    export CUDA_VISIBLE_DEVICES=${GPU_IDS}
    export VLLM_USE_RUST_FRONTEND=1
    export VLLM_API_KEY=${API_KEY}

    echo -e "${YELLOW}正在启动 vLLM 服务，请耐心等待模型加载...${NC}"
    nohup vllm serve ${MODEL_NAME} \
      --served-model-name ${SERVED_MODEL_NAME} \
      --trust-remote-code \
      --port ${PORT} \
      --host ${SERVER_IP} \
      --tensor-parallel-size ${TENSOR_PARALLEL_SIZE} \
      --dtype ${DTYPE} \
      --kv-cache-dtype ${KV_CACHE_DTYPE} \
      --block-size ${BLOCK_SIZE} \
      --gpu-memory-utilization ${GPU_MEMORY_UTIL} \
      --max-model-len ${MAX_MODEL_LEN} \
      --max-num-batched-tokens ${MAX_NUM_BATCHED_TOKENS} \
      --enforce-eager \
      --enable-expert-parallel \
      --tokenizer-mode deepseek_v4 \
      --tool-call-parser deepseek_v4 \
      --enable-auto-tool-choice \
      --reasoning-parser deepseek_v4 \
      --speculative-config '{"method":"dspark","num_speculative_tokens":7,"draft_sample_method":"probabilistic"}' \
      --api-key ${API_KEY} \
      > ${LOG_FILE} 2>&1 &

    PID=$!
    echo -e "${GREEN}✓ 服务已启动 (PID: ${PID})${NC}"
    echo -e "${GREEN}✓ 日志文件: ${LOG_FILE}${NC}"
    echo ""
    echo "等待服务就绪..."
    for i in {1..120}; do
        if check_service_http; then
            echo ""
            echo -e "${GREEN}✓ 服务已就绪！${NC}"
            break
        fi
        if ! kill -0 $PID 2>/dev/null; then
            echo ""
            echo -e "${RED}✗ 进程已退出，请查看日志${NC}"
            break
        fi
        echo -ne "\r等待中... [${i}/120]"
        sleep 2
    done
    echo ""
    if ! check_service_http; then
        echo -e "${RED}✗ 服务启动超时，请查看日志：tail -f ${LOG_FILE}${NC}"
    fi
    echo ""
    read -p "按 Enter 返回主菜单..."
}

stop_service() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    停止 DeepSeek-V4 服务${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""
    pkill -9 -f "VLLM::Worker" 2>/dev/null
    pkill -9 -f "vllm.*DeepSeek" 2>/dev/null
    sleep 2
    fuser -k /dev/nvidia* 2>/dev/null
    rm -rf /dev/shm/* 2>/dev/null
    rm -rf /tmp/vllm_* 2>/dev/null
    echo ""
    echo -e "${GREEN}✓ 服务已停止${NC}"
    GPU_MEM=$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits 2>/dev/null | head -1)
    if [ -n "$GPU_MEM" ] && [ "$GPU_MEM" -gt 1000 ]; then
        echo ""
        echo -e "${YELLOW}⚠ 检测到显存残留 (${GPU_MEM}MiB)${NC}"
        echo "  建议: 选择菜单 4 强制清理，或选择菜单 7 重启服务器"
    fi
    echo ""
    read -p "按 Enter 返回主菜单..."
}

force_cleanup() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    强制清理${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""
    echo -e "${YELLOW}此操作将：${NC}"
    echo "  - 强制终止所有 VLLM 进程"
    echo "  - 停止 NVIDIA 后台服务"
    echo "  - 关闭持久化模式"
    echo "  - 重置 GPU"
    echo "  - 清理共享内存"
    echo ""
    echo -n "确认执行？[y/N]: "
    read -r answer
    if [[ ! "$answer" =~ ^[Yy]$ ]]; then
        echo "取消操作"
        sleep 1
        return
    fi
    echo ""
    cleanup_gpu_memory
    echo ""
    read -p "按 Enter 返回主菜单..."
}

show_model_info() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    模型信息${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""
    if ! check_service_http; then
        echo -e "${RED}✗ 服务未运行，请先启动服务！${NC}"
        echo ""
        read -p "按 Enter 返回主菜单..."
        return
    fi
    echo -e "${GREEN}✓ 服务正在运行${NC}"
    echo ""
    echo -e "${YELLOW}【模型列表】${NC}"
    echo "----------------------------------------"
    curl -s "${BASE_URL}/v1/models" -H "Authorization: Bearer ${API_KEY}" | python3 -m json.tool
    echo "----------------------------------------"
    echo ""
    echo -e "${YELLOW}【GPU 状态】${NC}"
    echo "----------------------------------------"
    nvidia-smi --query-gpu=index,name,memory.used,memory.total,utilization.gpu --format=csv
    echo "----------------------------------------"
    echo ""
    echo -e "${YELLOW}【VLLM Worker 进程】${NC}"
    echo "----------------------------------------"
    pgrep -a -f "VLLM::Worker" || echo "无"
    echo "----------------------------------------"
    echo ""
    echo -e "${YELLOW}【服务配置】${NC}"
    echo "----------------------------------------"
    LOCAL_IP=$(get_local_ip)
    if [ "$LOCAL_IP" = "0.0.0.0" ]; then
        API_DISPLAY="http://${SERVER_IP}:${PORT} (无法检测到有效IP，请手动替换)"
    else
        API_DISPLAY="http://${LOCAL_IP}:${PORT} (监听 ${SERVER_IP}:${PORT})"
    fi
    echo "  API 地址: ${API_DISPLAY}"
    echo "  端口: ${PORT}"
    echo "  API Key: ${API_KEY}"
    echo "  模型: ${SERVED_MODEL_NAME} (${MODEL_NAME})"
    echo "  TP 大小: ${TENSOR_PARALLEL_SIZE}"
    echo "  精度: ${DTYPE}"
    echo "  KV Cache 精度: ${KV_CACHE_DTYPE}"
    echo "  Block Size: ${BLOCK_SIZE}"
    echo "  上下文长度: ${MAX_MODEL_LEN}"
    echo "  每批最大 token 数: ${MAX_NUM_BATCHED_TOKENS}"
    echo "  并发请求数限制: 无 (由 vLLM 自动管理)"
    echo "  投机解码: DSpark (7 tokens, probabilistic)"
    echo "  专家并行: 启用"
    echo "  强制 Eager 模式: 是 (--enforce-eager)"
    echo "----------------------------------------"
    echo ""
    read -p "按 Enter 返回主菜单..."
}

view_logs() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    实时日志${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""
    if [ ! -f ${LOG_FILE} ]; then
        echo -e "${YELLOW}⚠ 日志文件不存在，请先启动服务${NC}"
        echo ""
        read -p "按 Enter 返回主菜单..."
        return
    fi
    echo -e "${GREEN}查看日志文件: ${LOG_FILE}${NC}"
    echo -e "${YELLOW}提示：按 Ctrl+C 退出日志查看${NC}"
    echo ""
    read -p "按 Enter 继续..."
    tail -f ${LOG_FILE}
}

ipmi_reboot() {
    clear
    echo -e "${RED}========================================${NC}"
    echo -e "${RED}    警告：此操作将重启服务器！${NC}"
    echo -e "${RED}========================================${NC}"
    echo ""
    echo -e "${YELLOW}通过 IPMI 重启服务器${NC}"
    echo "  BMC IP: ${IPMI_HOST}"
    echo "  用户: ${IPMI_USER}"
    echo ""
    echo -e "${RED}⚠ 警告：所有正在运行的服务都会中断！${NC}"
    echo ""
    echo -n "确认重启服务器？[y/N]: "
    read -r answer
    if [[ ! "$answer" =~ ^[Yy]$ ]]; then
        echo "取消操作"
        sleep 1
        return
    fi
    echo ""

    if ! command -v ipmitool &> /dev/null; then
        echo -e "${YELLOW}ipmitool 未安装。${NC}"
        if [ "$EUID" -eq 0 ]; then
            echo "正在安装 ipmitool (需要 sudo)..."
            sudo apt-get update && sudo apt-get install -y ipmitool
        else
            echo -e "${RED}您不是 root 用户，无法自动安装 ipmitool。${NC}"
            echo "请手动执行: sudo apt-get install ipmitool，或联系管理员。"
            echo "按 Enter 返回..."
            read
            return
        fi
    fi

    echo "检查 IPMI 连接..."
    if ! ipmitool -I lanplus -H ${IPMI_HOST} -U ${IPMI_USER} -P ${IPMI_PASS} chassis power status &>/dev/null; then
        echo -e "${RED}✗ 无法连接到 BMC${NC}"
        echo ""
        echo "请检查："
        echo "  1. BMC IP 是否正确: ${IPMI_HOST}"
        echo "  2. 用户名密码是否正确"
        echo ""
        read -p "按 Enter 返回..."
        return
    fi

    echo -e "${GREEN}✓ BMC 连接正常${NC}"
    echo ""
    echo -e "${RED}10 秒后重启服务器...${NC}"
    echo -n "按 Ctrl+C 取消"
    for i in {10..1}; do
        echo -ne "\r${i} 秒后重启... "
        sleep 1
    done
    echo ""
    echo -e "${RED}正在重启服务器...${NC}"
    ipmitool -I lanplus -H ${IPMI_HOST} -U ${IPMI_USER} -P ${IPMI_PASS} chassis power reset
    echo ""
    echo -e "${GREEN}✓ 重启命令已发送${NC}"
    echo -e "${YELLOW}服务器正在重启中，启动时间大概需要 5 分钟左右${NC}"
    echo -e "${YELLOW}请耐心等待,5 分钟后重新 SSH 连接,如连接不上,请用BMC ip访问服务器,查看状态${NC}"
    echo ""
    echo "连接断开..."
    sleep 3
    exit 0
}

update_vllm() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    更新 vLLM 推理软件${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""
    echo "将执行: uv pip install --upgrade vllm"
    echo ""
    echo -n "确认更新？[y/N]: "
    read -r answer
    if [[ ! "$answer" =~ ^[Yy]$ ]]; then
        echo "取消操作"
        sleep 1
        return
    fi
    echo ""
    echo "正在更新，请稍候..."
    source ${UV_VENV_PATH}/bin/activate
    uv pip install --upgrade vllm
    if [ $? -eq 0 ]; then
        echo -e "${GREEN}✓ 更新完成${NC}"
    else
        echo -e "${RED}✗ 更新失败，请检查错误信息${NC}"
    fi
    echo ""
    read -p "按 Enter 返回主菜单..."
}

check_environment() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    AI 环境检测${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""

    echo -e "${YELLOW}[1/4] 检测 uv...${NC}"
    export PATH="$HOME/.local/bin:$PATH"
    if command -v uv &> /dev/null; then
        echo -e "${GREEN}✓ uv 已安装: $(uv --version)${NC}"
    else
        echo -e "${RED}✗ uv 未安装${NC}"
        echo -n "是否安装 uv？[y/N]: "
        read -r answer
        if [[ "$answer" =~ ^[Yy]$ ]]; then
            echo "正在安装 uv..."
            curl -LsSf https://astral.sh/uv/install.sh | sh
            export PATH="$HOME/.local/bin:$PATH"
            if command -v uv &> /dev/null; then
                echo -e "${GREEN}✓ uv 安装完成${NC}"
            else
                echo -e "${RED}✗ uv 安装后仍无法使用，请手动将 $HOME/.local/bin 加入 PATH${NC}"
            fi
        fi
    fi
    echo ""

    echo -e "${YELLOW}[2/4] 检测 uv 虚拟环境 (${UV_VENV_PATH})...${NC}"
    if [ -d "${UV_VENV_PATH}" ] && [ -f "${UV_VENV_PATH}/bin/activate" ]; then
        echo -e "${GREEN}✓ 虚拟环境存在: ${UV_VENV_PATH}${NC}"
    else
        echo -e "${RED}✗ 虚拟环境不存在: ${UV_VENV_PATH}${NC}"
        echo -n "是否创建虚拟环境？[y/N]: "
        read -r answer
        if [[ "$answer" =~ ^[Yy]$ ]]; then
            echo "正在创建虚拟环境..."
            uv venv ${UV_VENV_PATH} --python 3.12.13
            if [ $? -eq 0 ]; then
                echo -e "${GREEN}✓ 虚拟环境创建完成${NC}"
            else
                echo -e "${RED}✗ 虚拟环境创建失败，请检查 Python 3.12.13 是否可用${NC}"
            fi
        fi
    fi
    echo ""

    echo -e "${YELLOW}[3/4] 检测 vLLM...${NC}"
    if [ -f "${UV_VENV_PATH}/bin/activate" ]; then
        source ${UV_VENV_PATH}/bin/activate
        if uv pip show vllm &> /dev/null; then
            VLLM_VERSION=$(uv pip show vllm | grep Version | awk '{print $2}')
            echo -e "${GREEN}✓ vLLM 已安装: ${VLLM_VERSION}${NC}"
        else
            echo -e "${RED}✗ vLLM 未安装${NC}"
            echo -n "是否安装 vLLM？[y/N]: "
            read -r answer
            if [[ "$answer" =~ ^[Yy]$ ]]; then
                echo "正在安装 vLLM..."
                uv pip install vllm
                if [ $? -eq 0 ]; then
                    echo -e "${GREEN}✓ vLLM 安装完成${NC}"
                else
                    echo -e "${RED}✗ vLLM 安装失败，请检查网络或依赖${NC}"
                fi
            fi
        fi
    else
        echo -e "${RED}✗ 虚拟环境不存在，请先创建虚拟环境${NC}"
    fi
    echo ""

    echo -e "${YELLOW}[4/4] 检测 DeepGEMM (FP8 MoE 内核)...${NC}"
    if [ -f "${UV_VENV_PATH}/bin/activate" ]; then
        source ${UV_VENV_PATH}/bin/activate
        if python3 -c "import deep_gemm" 2>/dev/null; then
            echo -e "${GREEN}✓ DeepGEMM 已安装${NC}"
        else
            echo -e "${RED}✗ DeepGEMM 未安装${NC}"
            echo -n "是否安装 DeepGEMM？[y/N]: "
            read -r answer
            if [[ "$answer" =~ ^[Yy]$ ]]; then
                echo "正在安装 DeepGEMM..."
                bash <(curl -fsSL https://raw.githubusercontent.com/vllm-project/vllm/main/tools/install_deepgemm.sh)
                if [ $? -eq 0 ]; then
                    echo -e "${GREEN}✓ DeepGEMM 安装完成${NC}"
                else
                    echo -e "${RED}✗ DeepGEMM 安装失败，请确保 CUDA 和编译工具已安装${NC}"
                fi
            fi
        fi
    else
        echo -e "${RED}✗ 虚拟环境不存在，请先创建虚拟环境${NC}"
    fi
    echo ""

    echo -e "${YELLOW}[额外] 检测 Hugging Face 镜像连通性...${NC}"
    if [ "$USE_HF_MIRROR" = true ]; then
        if curl -s -o /dev/null -w "%{http_code}" "${HF_MIRROR_URL}" | grep -q "200\|301\|302"; then
            echo -e "${GREEN}✓ 镜像 ${HF_MIRROR_URL} 可访问${NC}"
        else
            echo -e "${YELLOW}⚠ 镜像 ${HF_MIRROR_URL} 不可访问，请检查网络或更换镜像${NC}"
        fi
    else
        echo -e "${YELLOW}⚠ Hugging Face 镜像未启用 (USE_HF_MIRROR=false)${NC}"
    fi
    echo ""

    echo -e "${GREEN}✓ 环境检测完成${NC}"
    echo ""
    read -p "按 Enter 返回主菜单..."
}

# 主菜单
show_main_menu() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    DeepSeek-V4-Flash 管理脚本${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""
    echo -e "服务状态：$(get_service_status)"
    echo ""
    echo -e "${YELLOW}请选择操作：${NC}"
    echo "  1) 启动模型服务（前台模式,显示日志,用于测试,关闭窗口模型会关闭）"
    echo "  2) 启动模型服务（后台模式,运行后可以退出终端）"
    echo "  3) 查看模型运行信息"
    echo "  4) 强制清理（显存残留+进程）"
    echo "  5) 停止模型服务"
    echo "  6) 查看实时日志"
    echo "  7) 通过 IPMI 重启服务器"
    echo "  8) 更新vllm推理软件"
    echo "  9) AI环境检测"
    echo " 10) 退出"
    echo ""
    echo -n "请输入选项 [1-10]: "
}

# 主循环
main() {
    while true; do
        show_main_menu
        read main_choice
        
        case $main_choice in
            1) start_service_foreground ;;
            2) start_service_background ;;
            3) show_model_info ;;
            4) force_cleanup ;;
            5) stop_service ;;
            6) view_logs ;;
            7) ipmi_reboot ;;
            8) update_vllm ;;
            9) check_environment ;;
            10) clear; echo -e "${GREEN}再见！${NC}"; exit 0 ;;
            *) echo -e "${RED}无效选项${NC}"; sleep 1 ;;
        esac
    done
}

trap 'echo -e "\n${YELLOW}退出${NC}"; exit 0' INT

main
