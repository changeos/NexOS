/* ============================================================
   Pinia store — 计算（虚拟机列表）
   ============================================================ */

import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import {
    getVms,
    startVm,
    stopVm,
    deleteVm,
    ApiError,
    type Vm,
} from '@/api'

export const useComputeStore = defineStore('compute', () => {
    // —— state ——
    const vms = ref<Vm[]>([])
    const loading = ref(false)
    const error = ref<string | null>(null)
    const lastUpdated = ref<number | null>(null)

    // —— getters ——
    const vmCount = computed(() => vms.value.length)
    const runningCount = computed(
        () => vms.value.filter((v) => (v.status || '').toLowerCase() === 'running').length,
    )

    // —— actions ——
    async function fetchVms() {
        loading.value = true
        error.value = null
        try {
            vms.value = await getVms()
            lastUpdated.value = Date.now()
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        } finally {
            loading.value = false
        }
    }

    async function start(id: string) {
        try {
            await startVm(id)
            await fetchVms()
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        }
    }

    async function stop(id: string) {
        try {
            await stopVm(id)
            await fetchVms()
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        }
    }

    async function remove(id: string) {
        try {
            await deleteVm(id)
            await fetchVms()
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        }
    }

    return {
        vms,
        loading,
        error,
        lastUpdated,
        vmCount,
        runningCount,
        fetchVms,
        start,
        stop,
        remove,
    }
})
