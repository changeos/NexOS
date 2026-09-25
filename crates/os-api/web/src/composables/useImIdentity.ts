/**
 * useImIdentity —— IM 区块链身份（secp256k1 密钥对）+ 挑战-签名认证流。
 *
 * 契约（docs/IM_BLOCKCHAIN_AUTH_DESIGN.md §2，后端 handlers/im.rs）：
 *   身份 = secp256k1 压缩公钥（`0x` + 66 hex），用户名只能是公钥；
 *   私钥永不出客户端（localStorage `os-im-privkey`，服务器不存任何私钥）。
 *
 * 认证三步（ensureAuthenticated 内部串起）：
 *   1. POST /api/v1/im/auth/challenge {pubkey} → {nonce, expires_in, display_name}（60s 单次有效）
 *   2. 本地签名：sign(SHA-256(nonce 的 UTF-8 字节)) → 65 字节 r||s||v hex
 *      （@noble/secp256k1 v3 的 sign 默认 prehash=sha256，等价于
 *       sign(sha256(new TextEncoder().encode(nonce)), priv)；服务端 k256
 *       vk.verify(nonce.as_bytes(), sig) 内部同样做 SHA-256，逐字节兼容，
 *       v 字段服务端忽略）
 *   3. POST /api/v1/im/auth/verify {pubkey, nonce, signature}
 *      → {token, expires_in(24h), pubkey, display_name}
 *
 * token 存内存 + localStorage `os-im-token`（JSON 含过期时间，过期自动重走
 * 全流程）；单点登录：新 verify 顶掉旧 token（服务端行为）。
 *
 * 单例状态（模式同 useWallpaper.ts）：模块级 ref + 懒初始化，
 * 多组件调用共享同一份身份/会话。
 *
 * 泛化（批次 3）：密钥管理与签名以 `signNonceWithKey` / `ensurePrivkeyHex`
 * 导出为共享内核，useChainIdentity.ts 基于同一密钥对管理 NexHub 大厅的
 * 独立 token（本文件 IM 行为不变）。
 */
import { computed, ref } from 'vue';
import * as secp from '@noble/secp256k1';
import { hmac } from '@noble/hashes/hmac.js';
import { sha256 } from '@noble/hashes/sha2.js';
import { endpoints } from '@/api/client';

// 同步签名需显式接上哈希实现（secp v3 同步路径默认不内置）：
// RFC6979 确定性 nonce 依赖 hmacSha256，prehash 依赖 sha256。
secp.hashes.sha256 = (m) => sha256(m);
secp.hashes.hmacSha256 = (key, msg) => hmac(sha256, key, msg);

/** 私钥 hex（64 字符，无 0x）持久化 key。 */
const PRIVKEY_STORAGE_KEY = 'os-im-privkey';
/** IM token 持久化 key（JSON：{token, pubkey, expiresAt}，过期自动重认证）。 */
const TOKEN_STORAGE_KEY = 'os-im-token';
/** token 提前 60s 视为不新鲜（WS 重连/请求前预刷新，避开边界 401）。 */
const TOKEN_FRESH_MARGIN_MS = 60_000;

/** 一份 IM 身份（密钥对）。privkeyHex 仅存在本机，不入网络。 */
export interface ImIdentity {
  /** 私钥 hex（64 字符，无 0x 前缀）。 */
  privkeyHex: string;
  /** 压缩公钥（0x + 66 hex）——即 IM 用户名。 */
  pubkey: string;
}

/** 一次成功认证的会话。 */
export interface ImAuthSession {
  /** 64 hex IM token（REST Bearer / WS ?token=）。 */
  token: string;
  /** 所属公钥。 */
  pubkey: string;
  /** 派生 EVM 地址（0x + 40 hex）展示名。 */
  displayName: string;
  /** 过期时间（ms epoch）。 */
  expiresAt: number;
}

/** 身份/认证错误（code 供 UI 分支：no-identity → 引导初始化）。 */
export class ImIdentityError extends Error {
  code: 'no-identity' | 'invalid-privkey' | 'auth-failed';

  constructor(code: ImIdentityError['code'], message: string) {
    super(message);
    this.name = 'ImIdentityError';
    this.code = code;
  }
}

// —— 单例状态（模块级，跨组件共享）——
const privkeyHex = ref('');
const pubkey = ref('');
const token = ref('');
const tokenExpiresAt = ref(0);
const displayName = ref('');
const authenticating = ref(false);

let initialized = false;
/** 并发 ensureAuthenticated 去重（单飞：同一时刻最多一条认证流）。 */
let inFlight: Promise<ImAuthSession> | null = null;

// —— localStorage 读写（隐私模式等异常时静默降级内存态）——

function lsGet(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}
function lsSet(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    /* 仅内存生效 */
  }
}
function lsRemove(key: string): void {
  try {
    window.localStorage.removeItem(key);
  } catch {
    /* ignore */
  }
}

// —— 密钥派生 / 校验 ——

/** 派生压缩公钥（0x + 66 hex）；私钥非法时抛 ImIdentityError。 */
function derivePubkey(hex: string): string {
  try {
    const pub = secp.getPublicKey(secp.etc.hexToBytes(hex), true);
    return `0x${secp.etc.bytesToHex(pub)}`;
  } catch {
    throw new ImIdentityError('invalid-privkey', '私钥非法（应为 32 字节 secp256k1 私钥 hex）');
  }
}

/** 规范化私钥输入（容错 0x 前缀/大小写/空白）；非法抛 ImIdentityError。 */
function normalizePrivkey(input: string): string {
  const hex = input.trim().replace(/^0x/i, '').toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(hex)) {
    throw new ImIdentityError('invalid-privkey', '私钥格式非法：应为 64 位 hex（可带 0x 前缀）');
  }
  if (!secp.utils.isValidSecretKey(secp.etc.hexToBytes(hex))) {
    throw new ImIdentityError('invalid-privkey', '私钥超出 secp256k1 曲线阶范围');
  }
  return hex;
}

/** 采纳一个私钥：校验 → 更新单例 → 持久化 → 清掉旧身份的 token。 */
function adoptIdentity(normalizedHex: string): ImIdentity {
  const pk = derivePubkey(normalizedHex);
  privkeyHex.value = normalizedHex;
  pubkey.value = pk;
  lsSet(PRIVKEY_STORAGE_KEY, normalizedHex);
  clearSession();
  return { privkeyHex: normalizedHex, pubkey: pk };
}

// —— token 会话（内存 + localStorage 双层）——

/** 内存会话是否仍新鲜（提前 60s 余量）。 */
function sessionFromMemory(): ImAuthSession | null {
  if (!token.value || !pubkey.value) return null;
  if (Date.now() + TOKEN_FRESH_MARGIN_MS >= tokenExpiresAt.value) return null;
  return {
    token: token.value,
    pubkey: pubkey.value,
    displayName: displayName.value,
    expiresAt: tokenExpiresAt.value,
  };
}

function adoptSession(s: ImAuthSession): void {
  token.value = s.token;
  tokenExpiresAt.value = s.expiresAt;
  displayName.value = s.displayName;
  lsSet(TOKEN_STORAGE_KEY, JSON.stringify(s));
}

/** 清空会话（内存 + localStorage；不动私钥身份）。 */
function clearSession(): void {
  token.value = '';
  tokenExpiresAt.value = 0;
  displayName.value = '';
  lsRemove(TOKEN_STORAGE_KEY);
}

/** localStorage 里的会话（换过身份/过期/损坏 → null）。 */
function sessionFromStorage(): ImAuthSession | null {
  const raw = lsGet(TOKEN_STORAGE_KEY);
  if (!raw) return null;
  try {
    const rec = JSON.parse(raw) as Partial<ImAuthSession>;
    if (
      !rec ||
      typeof rec.token !== 'string' ||
      typeof rec.expiresAt !== 'number' ||
      !rec.token
    ) {
      return null;
    }
    // 会话必须属于当前公钥（换身份后旧 token 一律作废）
    if (!pubkey.value || rec.pubkey !== pubkey.value) return null;
    if (Date.now() + TOKEN_FRESH_MARGIN_MS >= rec.expiresAt) return null;
    return {
      token: rec.token,
      pubkey: rec.pubkey,
      displayName: typeof rec.displayName === 'string' ? rec.displayName : '',
      expiresAt: rec.expiresAt,
    };
  } catch {
    return null;
  }
}

// —— 认证流 ——

/**
 * 对挑战 nonce 签名（**共享内核**，纯函数）：65 字节 r||s||v hex（0x 前缀；
 * v 服务端忽略，恒 0）。IM 与 NexHub 大厅（useChainIdentity）同一签名格式——
 * 服务端两侧均为 k256 `vk.verify(nonce.as_bytes(), sig[..64])`（内部 SHA-256）。
 *
 * 服务端 k256 vk.verify(nonce.as_bytes(), sig[..64])（内部 SHA-256 摘要）；
 * 客户端等价：对 nonce UTF-8 字节 prehash(sha256) 后 ECDSA（secp v3 默认 prehash=true）。
 * 注意：noble 的 { format: 'recovered' } 布局是 v||r||s（v 在前），
 * 后端要 r||s||v——用 compact（r||s）再补 1 字节 v 即可。
 */
export function signNonceWithKey(privkey: string, nonce: string): string {
  const sig64 = secp.sign(new TextEncoder().encode(nonce), secp.etc.hexToBytes(privkey), {
    prehash: true,
  });
  const sig65 = new Uint8Array(65);
  sig65.set(sig64, 0);
  sig65[64] = 0; // v（恢复位）；服务端校验时忽略
  return `0x${secp.etc.bytesToHex(sig65)}`;
}

/** IM 认证流内部签名（固定用当前单例身份私钥）。 */
function signNonce(nonce: string): string {
  return signNonceWithKey(privkeyHex.value, nonce);
}

/** 挑战-签名-验证全流程（不含缓存判断）。 */
async function runAuthFlow(): Promise<ImAuthSession> {
  const pk = pubkey.value;
  const ch = await endpoints.imAuthChallenge(pk);
  const verified = await endpoints.imAuthVerify(pk, ch.nonce, signNonce(ch.nonce));
  const session: ImAuthSession = {
    token: verified.token,
    pubkey: verified.pubkey || pk,
    displayName: verified.display_name || ch.display_name,
    expiresAt: Date.now() + verified.expires_in * 1000,
  };
  adoptSession(session);
  return session;
}

/** 懒初始化：读私钥 → 派生公钥；恢复未过期会话。 */
function ensureInit(): void {
  if (initialized) return;
  initialized = true;
  const stored = lsGet(PRIVKEY_STORAGE_KEY);
  if (stored && /^[0-9a-f]{64}$/.test(stored)) {
    try {
      privkeyHex.value = stored;
      pubkey.value = derivePubkey(stored);
    } catch {
      privkeyHex.value = '';
      pubkey.value = '';
    }
  }
  const cached = sessionFromStorage();
  if (cached) {
    token.value = cached.token;
    tokenExpiresAt.value = cached.expiresAt;
    displayName.value = cached.displayName;
  }
}

/**
 * 确保已认证：新鲜 token 直接返回；否则（无 token / 过期 / 换身份）
 * 重走 challenge→sign→verify。无私钥时抛 ImIdentityError('no-identity')。
 * 并发调用单飞共享同一 Promise。
 */
async function ensureAuthenticated(): Promise<ImAuthSession> {
  ensureInit();
  const mem = sessionFromMemory();
  if (mem) return mem;
  if (!privkeyHex.value) {
    throw new ImIdentityError('no-identity', '尚未生成/导入 IM 身份私钥');
  }
  if (inFlight) return inFlight;
  authenticating.value = true;
  inFlight = (async () => {
    // 双检：等锁期间别的流可能已完成（内存命中即短路）
    const again = sessionFromMemory();
    if (again) return again;
    const cached = sessionFromStorage();
    if (cached) {
      adoptSession(cached);
      return cached;
    }
    try {
      return await runAuthFlow();
    } catch (e) {
      clearSession();
      throw e instanceof Error
        ? new ImIdentityError('auth-failed', `IM 认证失败：${e.message}`)
        : new ImIdentityError('auth-failed', 'IM 认证失败');
    }
  })().finally(() => {
    authenticating.value = false;
    inFlight = null;
  });
  return inFlight;
}

/** 强制下次 ensureAuthenticated 重走全流程（401 后调用；只清 token 不动身份）。 */
function forceReauth(): void {
  clearSession();
}

// —— 身份管理 ——

/** 本地随机生成新身份（旧身份与其 token 一并被替换）。 */
function generateIdentity(): ImIdentity {
  ensureInit();
  const sk = secp.utils.randomSecretKey();
  return adoptIdentity(secp.etc.bytesToHex(sk));
}

/** 导入私钥（hex，可带 0x/大小写/空白）；非法抛 ImIdentityError。 */
function importIdentity(input: string): ImIdentity {
  ensureInit();
  return adoptIdentity(normalizePrivkey(input));
}

/** 清除本机身份（私钥 + token；不可恢复，需重新生成/导入）。 */
function clearIdentity(): void {
  ensureInit();
  privkeyHex.value = '';
  pubkey.value = '';
  lsRemove(PRIVKEY_STORAGE_KEY);
  clearSession();
}

/** 当前内存 token 是否新鲜（WS 重连前的快速检查）。 */
function hasFreshToken(): boolean {
  return sessionFromMemory() !== null;
}

export function useImIdentity() {
  ensureInit();

  /** 是否已有身份（私钥就绪）。 */
  const hasIdentity = computed(() => privkeyHex.value !== '');

  return {
    // 状态（响应式单例）
    hasIdentity,
    pubkey,
    displayName,
    tokenExpiresAt,
    authenticating,
    // 身份管理
    generateIdentity,
    importIdentity,
    clearIdentity,
    // 认证
    ensureAuthenticated,
    forceReauth,
    hasFreshToken,
  };
}

// —— 共享内核导出（泛化，批次 3：NexHub 大厅链上身份复用；IM 自身行为不变）——
// 密钥对全机唯一（localStorage `os-im-privkey` 单例状态）：IM 与 NexHub 大厅
// 共用同一密钥对，但各自持有独立 token（服务端两侧 ChainAuth token 桶互不相通）。
// useChainIdentity.ts 经以下两个入口复用密钥与签名，不复制任何加密逻辑。

/** 取当前身份私钥 hex（触发懒初始化；无身份返回空串）。 */
export function ensurePrivkeyHex(): string {
  ensureInit();
  return privkeyHex.value;
}
