/* ============================================================
   OS System Web UI — REST API 封装（Axios 版）
   对齐 os-api 网关 25 路由（参考 crates/os-api/static/js/api.js）。
   全部同源（baseURL 为空），由 os-api 网关直接提供静态资源 + API。
   ============================================================ */

import axios, { type AxiosInstance, AxiosError } from 'axios'

/** API 错误类型（统一封装，携带 status / path 信息） */
export class ApiError extends Error {
    status?: number
    path?: string
    constructor(message: string, status?: number, path?: string) {
        super(message)
        this.name = 'ApiError'
        this.status = status
        this.path = path
    }
}

/** 从 Axios 错误中提取统一 ApiError */
function toApiError(err: unknown, path: string): ApiError {
    if (err instanceof AxiosError) {
        const status = err.response?.status
        let detail = ''
        const data = err.response?.data as unknown
        if (data && typeof data === 'object') {
            const d = data as Record<string, unknown>
            detail = (d.error as string) || (d.message as string) || JSON.stringify(data)
        } else if (typeof data === 'string' && data) {
            detail = data
        }
        const text = status
            ? `${status} ${err.response?.statusText || ''}${detail ? ' — ' + detail : ''}`.trim()
            : (err.message || 'Network error')
        return new ApiError(text, status, path)
    }
    return new ApiError((err as Error)?.message || 'Unknown error', undefined, path)
}

/** Axios 实例：同源调用，统一超时 + 错误转换 */
const http: AxiosInstance = axios.create({
    baseURL: '',            // 同源：网关在同一 origin 暴露静态资源与 API
    timeout: 15000,         // 15s 超时（与旧版 api.js 一致）
    headers: { Accept: 'application/json' },
})

// ============================================================
// 系统（handlers/system.rs）
// ============================================================
export interface SystemStatus {
    [key: string]: unknown
}
export interface Health {
    status?: string
    [key: string]: unknown
}
export interface Version {
    version?: string
    [key: string]: unknown
}
export interface VirtCheck {
    [key: string]: unknown
}

export const getStatus = () => http.get<SystemStatus>('/status').then((r) => r.data)
export const getHealth = () => http.get<Health>('/healthz').then((r) => r.data)
export const getVersion = () => http.get<Version>('/api/v1/version').then((r) => r.data)
export const getVirtCheck = () =>
    http.get<VirtCheck>('/api/v1/system/virt-check').then((r) => r.data)

// ============================================================
// 存储（handlers/storage.rs）
// ============================================================
export interface Pool {
    name?: string
    [key: string]: unknown
}
export interface Dataset {
    name?: string
    pool?: string
    [key: string]: unknown
}
export interface Snapshot {
    name?: string
    dataset?: string
    [key: string]: unknown
}

export const getPools = (pool?: string) =>
    http
        .get<Pool | Pool[]>(pool ? `/api/v1/pools/${encodeURIComponent(pool)}` : '/api/v1/pools')
        .then((r) => r.data)
export const getDatasets = (pool?: string) =>
    http
        .get<Dataset[]>(
            pool ? `/api/v1/datasets?pool=${encodeURIComponent(pool)}` : '/api/v1/datasets',
        )
        .then((r) => r.data)
export const getSnapshots = (dataset?: string) =>
    http
        .get<Snapshot[]>(
            dataset ? `/api/v1/snapshots?dataset=${encodeURIComponent(dataset)}` : '/api/v1/snapshots',
        )
        .then((r) => r.data)

// ============================================================
// 计算（handlers/compute.rs）
// ============================================================
export interface Vm {
    id?: string
    name?: string
    status?: string
    [key: string]: unknown
}

export const getVms = () => http.get<Vm[]>('/api/v1/vms').then((r) => r.data)
export const getVm = (id: string) =>
    http.get<Vm>(`/api/v1/vms/${encodeURIComponent(id)}`).then((r) => r.data)
export const startVm = (id: string) =>
    http.post(`/api/v1/vms/${encodeURIComponent(id)}/start`).then((r) => r.data)
export const stopVm = (id: string) =>
    http.post(`/api/v1/vms/${encodeURIComponent(id)}/stop`).then((r) => r.data)
export const deleteVm = (id: string) =>
    http.delete(`/api/v1/vms/${encodeURIComponent(id)}`).then((r) => r.data)

// ============================================================
// 共享（handlers/share.rs）
// ============================================================
export interface Share {
    name?: string
    [key: string]: unknown
}
export interface Export {
    [key: string]: unknown
}

export const getShares = () => http.get<Share[]>('/shares').then((r) => r.data)
export const getExports = () => http.get<Export[]>('/api/v1/exports').then((r) => r.data)

// 创建 / 删除共享（POST /shares；DELETE /shares/:id）—— 对齐 static/js/shares.js
export const createShare = (body: unknown) =>
    http.post<Share>('/shares', body).then((r) => r.data)
export const deleteShare = (id: string) =>
    http.delete<Share>(`/shares/${encodeURIComponent(id)}`).then((r) => r.data)

// ============================================================
// 用户（handlers/user.rs）
// ============================================================
export interface User {
    name?: string
    [key: string]: unknown
}

export const getUsers = (includeDisabled = false) =>
    http
        .get<User[]>('/api/v1/users' + (includeDisabled ? '?include_disabled=1' : ''))
        .then((r) => r.data)

// 创建 / 删除用户（POST /api/v1/users；DELETE /api/v1/users/:id）—— 对齐 static/js/users.js
export const createUser = (body: unknown) =>
    http.post<User>('/api/v1/users', body).then((r) => r.data)
export const deleteUser = (id: string) =>
    http.delete<User>(`/api/v1/users/${encodeURIComponent(id)}`).then((r) => r.data)

// ============================================================
// 节点（handlers/discover.rs）
// ============================================================
export interface Node {
    id?: string
    [key: string]: unknown
}

export const getNodes = () => http.get<Node[]>('/discover/nodes').then((r) => r.data)
export const getNode = (id: string) =>
    http.get<Node>(`/api/v1/nodes/${encodeURIComponent(id)}`).then((r) => r.data)

// ============================================================
// 兼容简写对象（endpoints.* 风格，便于迁移旧代码）
// ============================================================
export const api = {
    // 系统
    status: getStatus,
    health: getHealth,
    version: getVersion,
    virtCheck: getVirtCheck,
    // 存储
    pools: getPools,
    datasets: getDatasets,
    snapshots: getSnapshots,
    // 计算
    vms: getVms,
    vm: getVm,
    vmStart: startVm,
    vmStop: stopVm,
    vmDelete: deleteVm,
    // 共享
    shares: getShares,
    exports: getExports,
    createShare,
    deleteShare,
    // 用户
    users: getUsers,
    createUser,
    deleteUser,
    // 节点
    nodes: getNodes,
    node: getNode,
    // 错误转换工具
    toApiError,
}

export default api
