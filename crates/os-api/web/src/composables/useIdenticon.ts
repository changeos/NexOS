// =============================================================================
// useIdenticon —— 身份确定性头像（identicon）+ 0x 身份短显（composable 风格）。
//
// 背景：接入的成员多了，EVM 地址（0x+40hex）与压缩公钥（0x+66hex）肉眼看
// 全是 "0x…"，无法分辨谁是谁。本文件提供两件展示层工具（Chat / CodeHub /
// ApiGateway 三视图共用，不涉及任何后端语义）：
//   1. identiconSvg(id)——从身份字符串确定性生成的 GitHub/BTS 风格 5×5
//      左右镜像像素头像（SVG data URL）。同一身份永远得到同一图案（不可
//      自定义、不可伪造外观），不同身份大概率得到不同图案与配色。
//   2. shortIdentity(id)——0x 前缀身份缩略为 `0x**…后四位`（如
//      `0x**…b58a`）；非 0x 输入原样返回（超长截断规则保持各视图现状）。
//
// 算法（纯函数，无 crypto 依赖）：
//   - 散列：对 id 做多轮 FNV-1a（32 位），每轮以前一轮结果为种子，摊开成
//     16 个确定性字节（色相/饱和度/亮度各取数 + 15 个图案决策位）。
//   - 图案：GitHub identicon 同款 5×5 网格，只决策左 3 列 × 5 行 = 15 格，
//     第 4/5 列水平镜像第 2/1 列（左右对称，视觉上是"图腾"而非噪点）。
//   - 配色：HSL——色相取哈希（0-359），饱和度 45-65%，前景亮度 45-60%，
//     背景取同色相浅色（亮度 90-95%）衬托前景。
//   - 缓存：模块级 Map 单例（key = `id@size`），同 id 同尺寸只拼一次 SVG。
// =============================================================================

/** FNV-1a 32 位散列（str + 种子 → uint32；Math.imul 保证 32 位乘法语义）。 */
function fnv1a(str: string, seed: number): number {
  let h = seed >>> 0;
  for (let i = 0; i < str.length; i++) {
    h ^= str.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

/** 对 id 做 4 轮 FNV-1a（前一轮结果作下一轮种子 + 轮次混淆），摊开 16 字节。 */
function hashBytes(id: string): number[] {
  const bytes: number[] = [];
  let h = 0x811c9dc5; // FNV-1a offset basis
  for (let round = 0; round < 4; round++) {
    h = fnv1a(`${round}:${id}`, h);
    bytes.push((h >>> 24) & 0xff, (h >>> 16) & 0xff, (h >>> 8) & 0xff, h & 0xff);
  }
  return bytes;
}

/** id+size → SVG data URL 的单例缓存（同 id 两次调用返回同一字符串）。 */
const svgCache = new Map<string, string>();

/**
 * 生成身份 identicon（SVG data URL，可直接放进 <img :src>）。
 *
 * @param id   身份字符串（公钥 / EVM 地址 / agent:nexos-assistant 等——
 *             任意稳定字符串均可，同 id 必得同图）
 * @param size 渲染尺寸 px（只影响 svg width/height 属性；矢量可任意缩放）
 */
export function identiconSvg(id: string, size = 24): string {
  const key = `${id}@${size}`;
  const hit = svgCache.get(key);
  if (hit != null) return hit;

  const b = hashBytes(id);
  // —— 配色（HSL）：色相取哈希，饱和度 45-65%，前景亮度 45-60%，背景同色相浅色 ——
  const hue = (((b[0] ?? 0) << 8) | (b[1] ?? 0)) % 360;
  const sat = 45 + ((b[2] ?? 0) % 21); // 45..65
  const lightFg = 45 + ((b[3] ?? 0) % 16); // 45..60
  const lightBg = 90 + ((b[4] ?? 0) % 6); // 90..95
  const fg = `hsl(${hue}, ${sat}%, ${lightFg}%)`;
  const bg = `hsl(${hue}, ${sat}%, ${lightBg}%)`;

  // —— 图案：5×5 网格，左 3 列 × 5 行 = 15 个决策位（b[5]、b[6] 共 16 位），左右镜像 ——
  const rects: string[] = [];
  let bit = 0;
  for (let x = 0; x < 3; x++) {
    for (let y = 0; y < 5; y++) {
      const on = ((b[5 + (bit >> 3)] ?? 0) >> (bit & 7)) & 1;
      bit++;
      if (!on) continue;
      rects.push(`<rect x="${x}" y="${y}" width="1" height="1" fill="${fg}"/>`);
      const mx = 4 - x; // 镜像列（x=2 的中轴列只画一次）
      if (mx !== x) rects.push(`<rect x="${mx}" y="${y}" width="1" height="1" fill="${fg}"/>`);
    }
  }

  const svg =
    `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" ` +
    `viewBox="0 0 5 5" shape-rendering="crispEdges">` +
    `<rect width="5" height="5" fill="${bg}"/>${rects.join('')}</svg>`;
  const url = `data:image/svg+xml,${encodeURIComponent(svg)}`;
  svgCache.set(key, url);
  return url;
}

/**
 * 0x 身份短显：`0x**…后 tail 位`（如 `0x**…b58a`）。
 *
 * 仅转换 0x 前缀输入（EVM 地址 / 压缩公钥）；其余（普通用户名、
 * agent:nexos-assistant 等）原样返回——超长截断规则保持各视图现状。
 */
export function shortIdentity(id: string | null | undefined, tail = 4): string {
  const s = String(id ?? '');
  if (!/^0x/i.test(s)) return s;
  // 过短（0x + 不足 tail 位）没有缩略意义，原样返回
  if (s.length <= 2 + tail) return s;
  return `0x**…${s.slice(-tail)}`;
}
