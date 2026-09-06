#!/bin/bash

# ==================== 帮助信息 ====================
usage() {
    echo "用法: $0 --p <并发范围> [选项]"
    echo ""
    echo "必填参数:"
    echo "  --p <起始值-结束值>     测试的并发范围 (例如: 1-5)"
    echo ""
    echo "可选参数:"
    echo "  --port <端口号>         vLLM 服务监听的端口号 (默认: 8123)"
    echo "  --in-len <Token数>      随机输入序列的长度 (默认: 5120)"
    echo "  --out-len <Token数>     随机输出序列的长度 (默认: 25600)"
    echo "  -h, --help              显示此帮助信息并退出"
    echo ""
    echo "示例:"
    echo "  使用默认长度:  $0 --p 1-5 --port 8123"
    echo "  自定义长度:    $0 --p 1-5 --in-len 2048 --out-len 1024"
    exit 1
}

# ==================== 默认参数配置 ====================
PORT=8123
CONCURRENCY_RANGE=""
INPUT_LEN=5120
OUTPUT_LEN=25600

# ==================== 参数解析 ====================
while [[ "$#" -gt 0 ]]; do
    case $1 in
        --p) CONCURRENCY_RANGE="$2"; shift ;;
        --port) PORT="$2"; shift ;;
        --in-len) INPUT_LEN="$2"; shift ;;
        --out-len) OUTPUT_LEN="$2"; shift ;;
        -h|--help) usage ;;
        *) echo "❌ 未知参数: $1"; usage ;;
    esac
    shift
done

# 检查必填参数
if [ -z "$CONCURRENCY_RANGE" ]; then
    echo "❌ 错误: 必须指定并发范围 (--p)"
    usage
fi

# 解析并发范围
START=$(echo "$CONCURRENCY_RANGE" | cut -d'-' -f1)
END=$(echo "$CONCURRENCY_RANGE" | cut -d'-' -f2)

# 简单的数字校验
if ! [[ "$START" =~ ^[0-9]+$ ]] || ! [[ "$END" =~ ^[0-9]+$ ]]; then
    echo "❌ 错误: 并发范围格式不正确，请使用 '起始值-结束值' (例如: 1-5)"
    usage
fi

# ==================== 准备结果目录 ====================
TIMESTAMP=$(date +"%Y%m%d_%H%M%S")
RESULT_DIR="./benchmark_results_${TIMESTAMP}"
mkdir -p "$RESULT_DIR"

echo "📂 结果目录: $RESULT_DIR"
echo "🔌 端口: $PORT"
echo "🔢 并发范围: $START-$END"
echo "📏 输入长度: $INPUT_LEN | 输出长度: $OUTPUT_LEN"
echo "============================================================"

# ==================== 循环执行压测 ====================
for ((i=START; i<=END; i++)); do
    echo ""
    echo "[$i/$END] 正在测试并发数: $i"
    echo "----------------------------------------"
    
    # 激活虚拟环境并执行压测命令
    source ~/vllm/bin/activate && \
    HF_ENDPOINT=https://hf-mirror.com \
    vllm bench serve --backend openai-chat --endpoint /v1/chat/completions \
        --host localhost --port "$PORT" \
        --model Qwen/Qwen3.6-27B-FP8 --tokenizer Qwen/Qwen3.6-27B-FP8 \
        --dataset-name random \
        --random-input-len "$INPUT_LEN" \
        --random-output-len "$OUTPUT_LEN" \
        --request-rate inf \
        --max-concurrency "$i" \
        --num-prompts $((i * 2)) \
        2>&1 | tee "${RESULT_DIR}/benchmark_${i}.txt"
    
    echo "✅ 结果已保存到: ${RESULT_DIR}/benchmark_${i}.txt"
    
    # 如果不是最后一个测试，等待 5 秒
    if [ "$i" -lt "$END" ]; then
        echo "⏳ 等待 5 秒后继续..."
        sleep 5
    fi
done

echo ""
echo "============================================================"
echo "🎉 所有测试完成！结果保存在: $RESULT_DIR"
echo "📊 查看汇总: cat ${RESULT_DIR}/benchmark_*.txt | grep -A 20 'Serving Benchmark Result'"