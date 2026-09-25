/**
 * useChainIdentity —— 共享链上身份（secp256k1 密钥对）+ NexHub 大厅 token 会话。
 *
 * 契约（docs/MEDIA_GEN_AND_CHAIN_AUTH.md §C，后端 os-nexhub/nexhub_lobby.rs）：
 *   - **密钥对与 IM 完全共用**：同一 localStorage 私钥（`os-im-privkey`）、同一
 *     压缩公钥（`0x` + 66 hex）；在 IM 页生成/导入的密钥在大厅直接可用。
 *   - **token 独立**：服务端两侧挂独立 ChainAuth 实例（IM 的 token 在 NexHub
 *     不可用），故同一密钥对需经 `/api/v1/nexhub/auth/*` 另走一次三步认证；
 *     本地持久化 key `os-nexhub-token`（与 IM 的 `os-im-token` 互不干扰）。
 *
 * 认证三步（ensureNexhubToken 内部串起，同 IM 款）：
 *   1. POST /api/v1/nexhub/auth/challenge {pubkey} → {nonce, expires_in, display_name}
 *   2. 本地签名：signNonceWithKey（与 IM 同一共享内核，65 字节 r||s||v hex）
 *   3. POST /api/v1/nexhub/auth/verify {pubkey, nonce, signature}
 *      → {token, expires_in(24h), pubkey, display_name}
 *
 * 大厅写端点（publish/下架/purchase/bounty 全部）带
 * `Authorization: Bearer <nexhub token>`（经 client.ts NexhubOpts 覆盖全局
 * admin token 注入）；body 自报身份字段服务端一律反查覆盖。
 *
 * 展示名（EVM 地址）本地 keccak256 派生，与后端
 * `os_common::chain_auth::derive_display_name` 同规则：
 * `keccak256(未压缩公钥[1..])[12..]` → `0x` + 40 hex。
 *
 * 单例状态（模式同 useImIdentity.ts）：模块级 ref + 懒初始化 + 并发单飞。
 */
import { computed, ref } from 'vue';
import * as secp from '@noble/secp256k1';
import { keccak_256 } from '@noble/hashes/sha3.js';
import { endpoints } from '@/api/client';
import { ensurePrivkeyHex, signNonceWithKey, useImIdentity } from './useImIdentity';

/** NexHub token 持久化 key（JSON：{token, pubkey, displayName, expiresAt}）。 */
const TOKEN_STORAGE_KEY = 'os-nexhub-token';
/** token 提前 60s 视为不新鲜（请求前预刷新，避开边界 401）。 */
const TOKEN_FRESH_MARGIN_MS = 60_000;

/** 一次成功认证的 NexHub 会话（token + 归属公钥 + 展示名）。 */
export interface ChainAuthSession {
  /** 64 hex nexhub token（大厅写端点 Bearer）。 */
  token: string;
  /** 所属压缩公钥（0x + 66 hex）。 */
  pubkey: string;
  /** 派生 EVM 地址（0x + 40 hex）展示名。 */
  displayName: string;
  /** 过期时间（ms epoch）。 */
  expiresAt: number;
}

/** 链上身份错误（code 供 UI 分支：no-identity → 引导去 IM 页初始化）。 */
export class ChainIdentityError extends Error {
  code: 'no-identity' | 'auth-failed';

  constructor(code: ChainIdentityError['code'], message: string) {
    super(message);
    this.name = 'ChainIdentityError';
    this.code = code;
  }
}

// —— 单例状态（模块级，跨组件共享；密钥对状态在 useImIdentity 单例里）——
const token = ref('');
const tokenExpiresAt = ref(0);
/** 会话归属公钥（换身份后旧 nexhub token 一律作废）。 */
const sessionPubkey = ref('');
const displayName = ref('');
const authenticating = ref(false);

let initialized = false;
/** 并发 ensureNexhubToken 去重（单飞：同一时刻最多一条认证流）。 */
let inFlight: Promise<ChainAuthSession> | null = null;

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

// —— 会话（内存 + localStorage 双层；不动密钥身份）——

/** 当前身份公钥（共享单例，随 IM 页生成/导入/清除实时变化）。 */
function currentPubkey(): string {
  return useImIdentity().pubkey.value;
}

/** 内存会话是否仍新鲜且属于当前公钥。 */
function sessionFromMemory(): ChainAuthSession | null {
  if (!token.value || !sessionPubkey.value) return null;
  if (sessionPubkey.value !== currentPubkey()) return null;
  if (Date.now() + TOKEN_FRESH_MARGIN_MS >= tokenExpiresAt.value) return null;
  return {
    token: token.value,
    pubkey: sessionPubkey.value,
    displayName: displayName.value,
    expiresAt: tokenExpiresAt.value,
  };
}

function adoptSession(s: ChainAuthSession): void {
  token.value = s.token;
  tokenExpiresAt.value = s.expiresAt;
  sessionPubkey.value = s.pubkey;
  displayName.value = s.displayName;
  lsSet(TOKEN_STORAGE_KEY, JSON.stringify(s));
}

/** 清空会话（内存 + localStorage；不动密钥身份）。 */
function clearSession(): void {
  token.value = '';
  tokenExpiresAt.value = 0;
  sessionPubkey.value = '';
  displayName.value = '';
  lsRemove(TOKEN_STORAGE_KEY);
}

/** localStorage 里的会话（换过身份/过期/损坏 → null）。 */
function sessionFromStorage(): ChainAuthSession | null {
  const raw = lsGet(TOKEN_STORAGE_KEY);
  if (!raw) return null;
  try {
    const rec = JSON.parse(raw) as Partial<ChainAuthSession>;
    if (
      !rec ||
      typeof rec.token !== 'string' ||
      typeof rec.expiresAt !== 'number' ||
      !rec.token
    ) {
      return null;
    }
    // 会话必须属于当前公钥（IM 页换身份后旧 token 一律作废）
    if (!rec.pubkey || rec.pubkey !== currentPubkey()) return null;
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

/** 懒初始化：恢复未过期且归属当前身份的 nexhub 会话。 */
function ensureInit(): void {
  if (initialized) return;
  initialized = true;
  // 先触发密钥对懒初始化（ensurePrivkeyHex 内部调用 useImIdentity 的 ensureInit）
  ensurePrivkeyHex();
  const cached = sessionFromStorage();
  if (cached) adoptSession(cached);
}

// —— 认证流 ——

/** 挑战-签名-验证全流程（不含缓存判断；签名与 IM 共用同一共享内核）。 */
async function runAuthFlow(): Promise<ChainAuthSession> {
  const pk = currentPubkey();
  const priv = ensurePrivkeyHex();
  const ch = await endpoints.nexhubAuthChallenge(pk);
  const verified = await endpoints.nexhubAuthVerify(pk, ch.nonce, signNonceWithKey(priv, ch.nonce));
  const session: ChainAuthSession = {
    token: verified.token,
    pubkey: verified.pubkey || pk,
    displayName: verified.display_name || ch.display_name,
    expiresAt: Date.now() + verified.expires_in * 1000,
  };
  adoptSession(session);
  return session;
}

/**
 * 确保已拿到 nexhub token：新鲜 token 直接返回；否则（无 token / 过期 /
 * 换身份）重走 challenge→sign→verify。无私钥时抛 ChainIdentityError('no-identity')。
 * 并发调用单飞共享同一 Promise。
 */
async function ensureNexhubToken(): Promise<ChainAuthSession> {
  ensureInit();
  const mem = sessionFromMemory();
  if (mem) return mem;
  if (!ensurePrivkeyHex() || !currentPubkey()) {
    throw new ChainIdentityError('no-identity', '尚未生成/导入链上身份私钥（请先到 IM 页初始化）');
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
        ? new ChainIdentityError('auth-failed', `NexHub 身份认证失败：${e.message}`)
        : new ChainIdentityError('auth-failed', 'NexHub 身份认证失败');
    }
  })().finally(() => {
    authenticating.value = false;
    inFlight = null;
  });
  return inFlight;
}

/** 强制下次 ensureNexhubToken 重走全流程（401 后调用；只清 token 不动身份）。 */
function forceNexhubReauth(): void {
  clearSession();
}

/** 当前内存 nexhub token 是否新鲜。 */
function hasFreshNexhubToken(): boolean {
  return sessionFromMemory() !== null;
}

// —— EVM 展示名（本地派生，与后端 chain_auth::derive_display_name 同规则）——

/** keccak256(未压缩公钥[1..])[12..] → 0x + 40 hex；非法私钥返回空串。 */
export function deriveEvmAddress(privkey: string): string {
  try {
    // 未压缩公钥 = 0x04 || X || Y（65 字节），取 [1..] 后 keccak256
    const uncompressed = secp.getPublicKey(secp.etc.hexToBytes(privkey), false);
    const hash = keccak_256(uncompressed.slice(1));
    return `0x${secp.etc.bytesToHex(hash.slice(12))}`;
  } catch {
    return '';
  }
}

/**
 * 共享链上身份 + NexHub token 会话 composable。
 *
 * 密钥对状态直接复用 useImIdentity 单例（同一 localStorage key）：IM 页的
 * 生成/导入/清除实时反映到 hasIdentity/pubkey/evmAddress。
 */
export function useChainIdentity() {
  ensureInit();
  const { hasIdentity, pubkey } = useImIdentity();

  /** EVM 展示名（pubkey 响应式依赖：IM 页换身份自动重算；无身份空串）。 */
  const evmAddress = computed(() => {
    if (!pubkey.value) return '';
    const priv = ensurePrivkeyHex();
    return priv ? deriveEvmAddress(priv) : '';
  });

  return {
    // 共享密钥对（响应式单例，与 useImIdentity 同源）
    hasIdentity,
    pubkey,
    evmAddress,
    // NexHub token 会话
    nexhubDisplayName: displayName,
    nexhubTokenExpiresAt: tokenExpiresAt,
    nexhubAuthenticating: authenticating,
    ensureNexhubToken,
    forceNexhubReauth,
    hasFreshNexhubToken,
  };
}
