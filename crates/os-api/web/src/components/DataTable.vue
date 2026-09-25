<script setup lang="ts" generic="T">
// =============================================================================
// DataTable —— 通用表格组件
//
// 特性：
//   - 泛型 T（每行数据类型）
//   - 列定义（key 标识 + 标题 + 宽度 + 对齐 + 是否可排序 + 自定义取值器）
//   - 排序：点击可排序列表头切换 asc/desc/none（用自定义 accessor 取值比较）
//   - 自定义单元格：通过具名插槽 `cell-<key>` 覆盖默认取值渲染
//   - 空态：rows 为空时显示 emptyText
//   - 加载态：loading=true 时显示骨架行（不阻断渲染）
//
// 设计：复用 GNOME 设计 token（表头浅灰、圆角卡片、发丝分隔线）。
// =============================================================================
import { computed, ref, watch } from 'vue';
import type { Column } from './data-table';

const props = withDefaults(
  defineProps<{
    /** 列定义。 */
    columns: Column<T>[];
    /** 数据行。 */
    rows: T[];
    /** 空数据时提示文本。 */
    emptyText?: string;
    /** 加载中状态（显示骨架行）。 */
    loading?: boolean;
    /** 每行 class 生成器（可选，用于条件着色）。 */
    rowClass?: (row: T, index: number) => string | undefined;
    /** 行 key 字段名（默认 'id'）。 */
    rowKey?: string | ((row: T) => string);
  }>(),
  {
    emptyText: '暂无数据',
    loading: false,
    rowKey: 'id',
  },
);

const emit = defineEmits<{
  /** 用户点击行时触发（携带该行数据）。 */
  (e: 'row-click', row: T): void;
  /** 排序变化：key + 方向（'asc' | 'desc' | null）。 */
  (e: 'sort-change', payload: { key: string; direction: 'asc' | 'desc' | null }): void;
}>();

// —— 排序状态 ——
const sortKey = ref<string | null>(null);
const sortDir = ref<'asc' | 'desc' | null>(null);

/** 切换排序：none → asc → desc → none 循环。 */
function toggleSort(col: Column<T>): void {
  if (!col.sortable) return;
  if (sortKey.value !== col.key) {
    sortKey.value = col.key;
    sortDir.value = 'asc';
  } else if (sortDir.value === 'asc') {
    sortDir.value = 'desc';
  } else if (sortDir.value === 'desc') {
    sortKey.value = null;
    sortDir.value = null;
  } else {
    sortDir.value = 'asc';
  }
  emit('sort-change', { key: col.key, direction: sortDir.value });
}

/** 取列值（accessor 优先，否则 row[key]）。 */
function valueOf(col: Column<T>, row: T): unknown {
  if (col.accessor) return col.accessor(row);
  return (row as Record<string, unknown>)[col.key];
}

/** 排序后的行（拷贝，不改原数组）。 */
const sortedRows = computed<T[]>(() => {
  if (!sortKey.value || !sortDir.value) return props.rows;
  const col = props.columns.find((c) => c.key === sortKey.value);
  if (!col) return props.rows;
  const dir = sortDir.value === 'asc' ? 1 : -1;
  return [...props.rows].sort((a, b) => {
    const va = valueOf(col, a);
    const vb = valueOf(col, b);
    if (va == null && vb == null) return 0;
    if (va == null) return 1;
    if (vb == null) return -1;
    if (typeof va === 'number' && typeof vb === 'number') return (va - vb) * dir;
    return String(va).localeCompare(String(vb)) * dir;
  });
});

const sortArrow = computed<Record<string, 'asc' | 'desc' | null>>(() => {
  const m: Record<string, 'asc' | 'desc' | null> = {};
  if (sortKey.value && sortDir.value) m[sortKey.value] = sortDir.value;
  return m;
});

/** 行 key 取值（字符串）。 */
function rowKeyValue(row: T, index: number): string {
  if (typeof props.rowKey === 'function') return props.rowKey(row);
  const v = (row as Record<string, unknown>)[props.rowKey];
  return v != null ? String(v) : `__row_${index}`;
}

/** 骨架占位行（loading 时显示）。 */
const skeletonRows = computed(() => Array.from({ length: 4 }, (_, i) => i));

// rows 变化时若排序键已失效（列被移除），重置排序
watch(
  () => props.columns,
  (cols) => {
    if (sortKey.value && !cols.some((c) => c.key === sortKey.value)) {
      sortKey.value = null;
      sortDir.value = null;
    }
  },
);
</script>

<template>
  <div class="data-table-wrap">
    <table class="data-table">
      <colgroup>
        <col v-for="col in columns" :key="col.key" :style="col.width ? { width: col.width } : {}" />
      </colgroup>
      <thead>
        <tr>
          <th
            v-for="col in columns"
            :key="col.key"
            :class="[`align-${col.align ?? 'left'}`, { sortable: col.sortable }]"
            @click="col.sortable && toggleSort(col)"
          >
            <span class="th-label">
              {{ col.title }}
              <span v-if="col.sortable" class="sort-arrow" :class="sortArrow[col.key] ?? 'none'">
                <span v-if="sortArrow[col.key] === 'asc'">▲</span>
                <span v-else-if="sortArrow[col.key] === 'desc'">▼</span>
                <span v-else class="dim">⇅</span>
              </span>
            </span>
          </th>
        </tr>
      </thead>
      <tbody>
        <!-- 加载态：骨架行 -->
        <template v-if="loading">
          <tr v-for="i in skeletonRows" :key="`sk-${i}`" class="skeleton-row">
            <td v-for="col in columns" :key="col.key">
              <div class="skeleton-cell"></div>
            </td>
          </tr>
        </template>
        <!-- 空态 -->
        <tr v-else-if="!sortedRows.length">
          <td :colspan="columns.length" class="empty-row">{{ emptyText }}</td>
        </tr>
        <!-- 数据行 -->
        <template v-else>
          <tr
            v-for="(row, index) in sortedRows"
            :key="rowKeyValue(row, index)"
            :class="rowClass ? rowClass(row, index) : undefined"
            @click="emit('row-click', row)"
          >
            <td
              v-for="col in columns"
              :key="col.key"
              :class="[`align-${col.align ?? 'left'}`]"
            >
              <!-- 具名插槽 cell-<key> 覆盖默认渲染 -->
              <slot :name="`cell-${col.key}`" :row="row" :value="valueOf(col, row)" :index="index">
                {{ valueOf(col, row) ?? '—' }}
              </slot>
            </td>
          </tr>
        </template>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
.data-table-wrap {
  width: 100%;
  overflow-x: auto;
}

.data-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 14px;
  color: var(--text, #2B2B2B);
}

.data-table thead th {
  background: var(--bg-app, #FAFAFA);
  text-align: left;
  padding: 10px 14px;
  font-weight: 600;
  font-size: 12.5px;
  color: var(--text-muted, #5E5C5F);
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  white-space: nowrap;
  letter-spacing: -0.01em;
  user-select: none;
}

.data-table thead th.sortable {
  cursor: pointer;
}

.data-table thead th.sortable:hover .th-label {
  color: var(--accent, #E95420);
}

.data-table tbody td {
  padding: 12px 14px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  vertical-align: middle;
}

.data-table tbody tr:last-child td {
  border-bottom: none;
}

.data-table tbody tr {
  transition: background 0.12s ease;
  cursor: default;
}

.data-table tbody tr:hover {
  background: rgba(0, 0, 0, 0.025);
}

.align-left {
  text-align: left;
}
.align-center {
  text-align: center;
}
.align-right {
  text-align: right;
}

/* 让对齐类作用到表头文本 */
th.align-center .th-label,
th.align-right .th-label {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.th-label {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.sort-arrow {
  font-size: 10px;
  color: var(--accent, #E95420);
  line-height: 1;
}

.sort-arrow .dim {
  color: var(--text-faint, #aeaeb2);
  opacity: 0.6;
}

.empty-row {
  text-align: center !important;
  padding: 32px 14px;
  color: var(--text-muted, #5E5C5F);
  font-size: 13px;
}

/* —— 骨架屏 —— */
.skeleton-row td {
  padding: 14px;
}

.skeleton-cell {
  height: 14px;
  width: 70%;
  border-radius: 6px;
  background: linear-gradient(
    90deg,
    rgba(0, 0, 0, 0.05) 25%,
    rgba(0, 0, 0, 0.08) 37%,
    rgba(0, 0, 0, 0.05) 63%
  );
  background-size: 400% 100%;
  animation: skeleton-shimmer 1.4s ease infinite;
}

@keyframes skeleton-shimmer {
  0% {
    background-position: 100% 50%;
  }
  100% {
    background-position: 0 50%;
  }
}

@media (prefers-reduced-motion: reduce) {
  .skeleton-cell {
    animation: none;
  }
}
</style>
