// =============================================================================
// 剪贴板工具（兼容 HTTP 非安全上下文）
// =============================================================================

/** 复制文本到剪贴板：安全上下文走 Clipboard API；HTTP 等非安全上下文
 *  （navigator.clipboard undefined）回退临时 textarea + execCommand('copy')。 */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    /* 安全上下文但权限被拒等 → 走回退 */
  }
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  ta.style.pointerEvents = 'none';
  document.body.appendChild(ta);
  ta.focus();
  ta.select();
  try {
    return document.execCommand('copy');
  } finally {
    ta.remove();
  }
}
