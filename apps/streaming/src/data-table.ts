// =============================================================================
// DataTable 共享类型定义（独立 .ts 文件，便于消费者 import 复用）
//
// 放在独立文件而非 DataTable.vue 内，是因为 Vue SFC 的 <script setup> 默认不
// 导出类型；独立模块可被 vue-tsc + Vite 稳定解析。
// =============================================================================

/** 单元格水平对齐。 */
export type Align = 'left' | 'center' | 'right';

/**
 * 列定义（泛型 T = 行数据类型）。
 * accessor 用于排序与默认取值；不提供则取 row[key]。
 */
export interface Column<T> {
  /** 列标识（也是默认取值的字段名 + 具名插槽名 cell-<key>）。 */
  key: string;
  /** 表头标题。 */
  title: string;
  /** 列宽（CSS 宽度，如 '180px'）。 */
  width?: string;
  /** 水平对齐（默认 left）。 */
  align?: Align;
  /** 是否可排序（默认 false）。 */
  sortable?: boolean;
  /** 自定义取值器（用于排序与默认单元格显示；不提供则取 row[key]）。 */
  accessor?: (row: T) => unknown;
}
