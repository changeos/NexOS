#!/bin/bash

# Qwen3.6-27B 交互式管理脚本（使用 uv 虚拟环境）

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# ==================== 配置区域 ====================
# 模型配置
MODEL_NAME="Qwen/Qwen3.6-27B-FP8"
SERVED_MODEL_NAME="Qwen3.6-27B"
SERVER_IP="0.0.0.0"
PORT="8123"
BASE_URL="http://${SERVER_IP}:${PORT}"
LOG_FILE="/tmp/qwen3.6_vllm.log"

# GPU 配置（8 张NVIDIA GPU）
GPU_IDS="0"
TENSOR_PARALLEL_SIZE=1

# uv 虚拟环境路径（请根据实际路径修改）
UV_VENV_PATH="$HOME/vllm"   # 对应 uv venv ~/vllm --python 3.12

# IPMI 配置（可选，用于远程重启服务器）
IPMI_HOST="10.10.3.159"
IPMI_USER="admin"
IPMI_PASS="admin"

# vLLM 启动参数（可根据需要调整）
MAX_MODEL_LEN=262144                   # 256K 上下文
GPU_MEMORY_UTIL=0.95                   # GPU 显存利用率
DTYPE="auto"                       # 可选 bfloat16 / float16
SPEC_TOKENS=2                          # MTP 投机解码 token 数
# ================================================

# 获取本机活动 IPv4 地址（第一个非环回接口）
get_local_ip() {
    # 优先使用 hostname -I，取第一个IP
    local ip=$(hostname -I 2>/dev/null | awk '{print $1}')
    if [ -n "$ip" ] && [ "$ip" != "127.0.0.1" ]; then
        echo "$ip"
        return
    fi
    # 备选：通过 ip 命令获取
    ip=$(ip -4 addr show scope global 2>/dev/null | grep inet | awk '{print $2}' | cut -d/ -f1 | head -1)
    if [ -n "$ip" ]; then
        echo "$ip"
        return
    fi
    # 若均失败，回退到 0.0.0.0
    echo "0.0.0.0"
}

# 检查服务是否运行（通过进程）
check_service_by_process() {
    if pgrep -f "VLLM::Worker" > /dev/null || pgrep -f "vllm.*${MODEL_NAME}" > /dev/null; then
        return 0
    else
        return 1
    fi
}

# 检查服务是否可响应 HTTP 请求
check_service_http() {
    if curl -s -f -o /dev/null "${BASE_URL}/health" 2>/dev/null; then
        return 0
    else
        return 1
    fi
}

# 获取服务状态（文字描述）
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

# 强制清理 GPU 显存和残留进程
cleanup_gpu_memory() {
    echo "清理 GPU 显存残留..."
    
    echo "  停止 NVIDIA 后台服务..."
    systemctl stop nvidia-persistenced 2>/dev/null
    systemctl stop nvidia-fabricmanager 2>/dev/null
    
    nvidia-smi -pm 0 2>/dev/null
    
    echo "  终止 VLLM 进程..."
    pkill -9 -f "VLLM::Worker" 2>/dev/null
    pkill -9 -f "vllm.*Qwen" 2>/dev/null
    pkill -9 -f "python.*vllm" 2>/dev/null
    
    echo "  清理设备文件..."
    fuser -k /dev/nvidia* 2>/dev/null
    
    sleep 2
    
    echo "  重置 GPU..."
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

# 前台启动（显示日志，Ctrl+C 停止）
start_service_foreground() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    启动 Qwen3.6-27B 服务${NC}"
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
    
    # 激活 uv 虚拟环境
    source ${UV_VENV_PATH}/bin/activate
    
    export VLLM_WORKER_MULTIPROC_METHOD=spawn
    export CUDA_VISIBLE_DEVICES=${GPU_IDS}
    export VLLM_USE_RUST_FRONTEND=0
    
    vllm serve ${MODEL_NAME} \
      --served-model-name ${SERVED_MODEL_NAME} \
      --trust-remote-code \
      --port ${PORT} \
      --host ${SERVER_IP} \
      --tensor-parallel-size ${TENSOR_PARALLEL_SIZE} \
      --dtype ${DTYPE} \
      --max-model-len ${MAX_MODEL_LEN} \
      --gpu-memory-utilization ${GPU_MEMORY_UTIL} \
      --enable-auto-tool-choice \
      --tool-call-parser qwen3_coder \
      --reasoning-parser qwen3 \
      --mm-encoder-tp-mode data \
      --speculative-config '{"method":"mtp","num_speculative_tokens":'${SPEC_TOKENS}'}' \
      --log-stats
    
    echo ""
    echo -e "${YELLOW}服务已停止${NC}"
    sleep 2
}

# 后台启动
start_service_background() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    启动 Qwen3.6-27B 服务${NC}"
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
    
    # 激活 uv 虚拟环境（在 nohup 中也要生效，因此先导出环境变量）
    source ${UV_VENV_PATH}/bin/activate
    
    export VLLM_WORKER_MULTIPROC_METHOD=spawn
    export CUDA_VISIBLE_DEVICES=${GPU_IDS}
    export VLLM_USE_RUST_FRONTEND=1
    
    nohup vllm serve ${MODEL_NAME} \
      --served-model-name ${SERVED_MODEL_NAME} \
      --trust-remote-code \
      --port ${PORT} \
      --host ${SERVER_IP} \
      --tensor-parallel-size ${TENSOR_PARALLEL_SIZE} \
      --dtype ${DTYPE} \
      --max-model-len ${MAX_MODEL_LEN} \
      --gpu-memory-utilization ${GPU_MEMORY_UTIL} \
      --enable-auto-tool-choice \
      --tool-call-parser qwen3_coder \
      --reasoning-parser qwen3 \
      --mm-encoder-tp-mode data \
      --speculative-config '{"method":"mtp","num_speculative_tokens":'${SPEC_TOKENS}'}' \
      --log-stats \
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

# 停止服务
stop_service() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    停止 Qwen3.6-27B 服务${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""
    
    echo "1. 终止 VLLM Worker 进程..."
    pkill -9 -f "VLLM::Worker" 2>/dev/null
    pkill -9 -f "vllm.*Qwen" 2>/dev/null
    
    sleep 2
    
    echo "2. 清理设备文件占用..."
    fuser -k /dev/nvidia* 2>/dev/null
    
    echo "3. 清理共享内存..."
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

# 强制清理菜单
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

# 查看实时日志
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

# 显示模型信息、GPU 状态、配置
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
    curl -s "${BASE_URL}/v1/models" | python3 -m json.tool
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
    # 获取本地活动IP
    LOCAL_IP=$(get_local_ip)
    if [ "$LOCAL_IP" = "0.0.0.0" ]; then
        API_DISPLAY="http://${SERVER_IP}:${PORT} (无法检测到有效IP，请手动替换)"
    else
        API_DISPLAY="http://${LOCAL_IP}:${PORT} (监听 ${SERVER_IP}:${PORT})"
    fi
    echo "  API 地址: ${API_DISPLAY}"
    echo "  端口: ${PORT}"
    echo "  模型: ${SERVED_MODEL_NAME} (${MODEL_NAME})"
    echo "  TP 大小: ${TENSOR_PARALLEL_SIZE}"
    echo "  上下文长度: ${MAX_MODEL_LEN}"
    echo "  精度: ${DTYPE}"
    echo "  MTP token 数: ${SPEC_TOKENS}"
    echo "  Prometheus 指标: /metrics (已启用 --log-stats)"
    echo "----------------------------------------"
    
    echo ""
    read -p "按 Enter 返回主菜单..."
}

# 通过 IPMI 重启服务器（需要 ipmitool）
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
        echo "ipmitool 未安装，正在安装..."
        sudo apt-get update && sudo apt-get install -y ipmitool
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

# 主菜单
show_main_menu() {
    clear
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}    Qwen3.6-27B 管理脚本${NC}"
    echo -e "${BLUE}========================================${NC}"
    echo ""
    echo -e "服务状态：$(get_service_status)"
    echo ""
    echo -e "${YELLOW}请选择操作：${NC}"
    echo "  1) 启动模型服务（前台模式,显示日志,用于测试,关闭窗口模型会关闭）"
    echo "  2) 启动模型服务（后台模式,运行后可以退出终端）"
    echo "  3) 停止模型服务"
    echo "  4) 强制清理（显存残留+进程）"
    echo "  5) 查看模型运行信息"
    echo "  6) 查看实时日志"
    echo "  7) 通过 IPMI 重启服务器"
    echo "  8) 退出"
    echo ""
    echo -n "请输入选项 [1-8]: "
}

# 主循环
main() {
    while true; do
        show_main_menu
        read main_choice
        
        case $main_choice in
            1) start_service_foreground ;;
            2) start_service_background ;;
            3) stop_service ;;
            4) force_cleanup ;;
            5) show_model_info ;;
            6) view_logs ;;
            7) ipmi_reboot ;;
            8) clear; echo -e "${GREEN}再见！${NC}"; exit 0 ;;
            *) echo -e "${RED}无效选项${NC}"; sleep 1 ;;
        esac
    done
}

trap 'echo -e "\n${YELLOW}退出${NC}"; exit 0' INT

main