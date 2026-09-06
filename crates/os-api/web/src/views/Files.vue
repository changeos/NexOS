<script setup lang="ts">
// =============================================================================
// Files.vue —— 文件管理页
//
// 浏览与管理逻辑已抽到共享组件 @/components/FileBrowser.vue：
//   面包屑导航、整行进目录、排序、5s 静默轮询、目录用量懒加载、
//   新建文件夹 / 删除（两步确认）/ 重命名 等全部能力。
// 本页只负责页面标题与起始根（root="/" = 后端根映射 /tank）。
//
// TODO(上传/下载)：本期范围外，详见 FileBrowser.vue 文件头注释。
// =============================================================================
import FileBrowser from '@/components/FileBrowser.vue';
</script>

<template>
  <div class="files-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">文件管理</h2>
        <div class="page-sub muted">浏览与管理 OS 文件系统（根目录映射 /tank）</div>
      </div>
    </div>

    <!-- 浏览 + 写操作（新建/删除/重命名）全部由 FileBrowser 承担 -->
    <FileBrowser root="/" />
  </div>
</template>

<style scoped>
.files-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 18px;
}

.page-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}
.page-title {
  font-size: 22px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
  letter-spacing: -0.02em;
}
.page-sub { margin-top: 4px; font-size: 13px; }
.muted { color: var(--text-muted, #5E5C5F); }
</style>
