/* ============================================================
   Pinia store — 存储（pools / datasets / snapshots）
   ============================================================ */

import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import {
    getPools,
    getDatasets,
    getSnapshots,
    ApiError,
    type Pool,
    type Dataset,
    type Snapshot,
} from '@/api'

/** 把可能是单对象或数组的返回统一成数组 */
function toArray<T>(v: T | T[] | null | undefined): T[] {
    if (v == null) return []
    return Array.isArray(v) ? v : [v]
}

/**
 * ZFS 工具不可用时后端返回 200 空态对象（`{<key>:[], zfs_available:false}`，
 * 见 storage.rs「无 ZFS 节点降级」）——不是错误，按空列表处理，不塞进 error。
 */
function isZfsDegraded(v: unknown): boolean {
    return (
        !!v &&
        typeof v === 'object' &&
        !Array.isArray(v) &&
        (v as Record<string, unknown>).zfs_available === false
    )
}

export const useStorageStore = defineStore('storage', () => {
    // —— state ——
    const pools = ref<Pool[]>([])
    const datasets = ref<Dataset[]>([])
    const snapshots = ref<Snapshot[]>([])

    const loading = ref(false)
    const error = ref<string | null>(null)
    const lastUpdated = ref<number | null>(null)

    /** 当前过滤条件（可选） */
    const poolFilter = ref<string | undefined>(undefined)
    const datasetFilter = ref<string | undefined>(undefined)

    // —— getters ——
    const poolCount = computed(() => pools.value.length)
    const datasetCount = computed(() => datasets.value.length)
    const snapshotCount = computed(() => snapshots.value.length)

    // —— actions ——
    async function fetchPools() {
        try {
            const data = await getPools(poolFilter.value)
            pools.value = isZfsDegraded(data) ? [] : toArray(data)
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        }
    }

    async function fetchDatasets() {
        try {
            const data = await getDatasets(poolFilter.value)
            datasets.value = isZfsDegraded(data) ? [] : toArray(data)
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        }
    }

    async function fetchSnapshots() {
        try {
            const data = await getSnapshots(datasetFilter.value)
            snapshots.value = isZfsDegraded(data) ? [] : toArray(data)
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        }
    }

    /** 拉取全部存储资源 */
    async function fetchAll() {
        loading.value = true
        error.value = null
        try {
            await Promise.allSettled([fetchPools(), fetchDatasets(), fetchSnapshots()])
            lastUpdated.value = Date.now()
        } finally {
            loading.value = false
        }
    }

    function setPoolFilter(pool?: string) {
        poolFilter.value = pool
    }
    function setDatasetFilter(ds?: string) {
        datasetFilter.value = ds
    }

    return {
        pools,
        datasets,
        snapshots,
        loading,
        error,
        lastUpdated,
        poolFilter,
        datasetFilter,
        poolCount,
        datasetCount,
        snapshotCount,
        fetchPools,
        fetchDatasets,
        fetchSnapshots,
        fetchAll,
        setPoolFilter,
        setDatasetFilter,
    }
})
