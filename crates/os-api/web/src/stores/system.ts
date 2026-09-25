/* ============================================================
   Pinia store — 系统状态（status / health / version）
   ============================================================ */

import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import {
    getStatus,
    getHealth,
    getVersion,
    getVirtCheck,
    ApiError,
    type SystemStatus,
    type Health,
    type Version,
    type VirtCheck,
} from '@/api'

export const useSystemStore = defineStore('system', () => {
    // —— state ——
    const status = ref<SystemStatus | null>(null)
    const health = ref<Health | null>(null)
    const version = ref<Version | null>(null)
    const virtCheck = ref<VirtCheck | null>(null)

    const loading = ref(false)
    const error = ref<string | null>(null)
    const lastUpdated = ref<number | null>(null)

    // —— getters ——
    /** 健康状态：ok / warn / err / unknown */
    const healthLevel = computed<'ok' | 'warn' | 'err' | 'unknown'>(() => {
        const s = (health.value?.status || '').toLowerCase()
        if (s === 'healthy' || s === 'ok') return 'ok'
        if (s === 'degraded' || s === 'warn') return 'warn'
        if (s === 'unhealthy' || s === 'err' || s === 'error') return 'err'
        return 'unknown'
    })

    const versionString = computed(() => version.value?.version ?? '—')
    const isHealthy = computed(() => healthLevel.value === 'ok')

    // —— actions ——
    async function fetchStatus() {
        try {
            status.value = await getStatus()
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        }
    }

    async function fetchHealth() {
        try {
            health.value = await getHealth()
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        }
    }

    async function fetchVersion() {
        try {
            version.value = await getVersion()
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        }
    }

    async function fetchVirtCheck() {
        try {
            virtCheck.value = await getVirtCheck()
        } catch (e) {
            error.value = (e as ApiError).message
            throw e
        }
    }

    /** 拉取系统概览（status + health + version） */
    async function fetchOverview() {
        loading.value = true
        error.value = null
        try {
            await Promise.allSettled([fetchStatus(), fetchHealth(), fetchVersion()])
            lastUpdated.value = Date.now()
        } finally {
            loading.value = false
        }
    }

    return {
        status,
        health,
        version,
        virtCheck,
        loading,
        error,
        lastUpdated,
        healthLevel,
        versionString,
        isHealthy,
        fetchStatus,
        fetchHealth,
        fetchVersion,
        fetchVirtCheck,
        fetchOverview,
    }
})
