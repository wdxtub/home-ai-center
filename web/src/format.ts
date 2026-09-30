/** 金额与时间的显示口径。
 *
 * 金额全程是「微元」整数（1e-6 元）。显示时才转元：
 * 直接拿整数当「分」显示会差 100 倍，这��错一次就很难被用户发现。
 */

/** 微元 → 元，保留 4 位小数（家用量级够用，也不会把 0 显示成 0.00 骗人）。 */
export function microToYuan(micro: number): string {
  return (micro / 1_000_000).toFixed(4)
}

/** 微元 → 「元」+ 单位，用于卡片展示。 */
export function yuan(micro: number): string {
  return `${microToYuan(micro)} 元`
}

/** 表格里紧凑一点：小于 1 元时显示元，大额显示元（家用量级不需要 K/M）。 */
export function money(micro: number): string {
  const y = micro / 1_000_000
  if (Math.abs(y) >= 0.0001) return `${y.toFixed(4)}`
  return micro.toString()
}

export function yuanTime(unixSec: number | null | undefined): string {
  if (!unixSec) return '—'
  return new Date(unixSec * 1000).toLocaleString('zh-CN', { hour12: false })
}

export function timeOnly(unixSec: number | null | undefined): string {
  if (!unixSec) return '—'
  return new Date(unixSec * 1000).toLocaleTimeString('zh-CN', { hour12: false })
}

/** 毫秒 → 人类可读。排队与延迟都用它。 */
export function ms(v: number | null | undefined): string {
  if (v == null) return '—'
  if (v < 1000) return `${v} ms`
  if (v < 60_000) return `${(v / 1000).toFixed(1)} s`
  return `${Math.floor(v / 60_000)}m ${Math.round((v % 60_000) / 1000)}s`
}

export function bytes(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

/** 冷却剩余秒 → 人类可读。 */
export function cooldown(secs: number): string {
  if (secs <= 0) return '—'
  if (secs < 60) return `${secs}s`
  if (secs < 3600) return `${Math.round(secs / 60)}m`
  return `${(secs / 3600).toFixed(1)}h`
}

/** 把 unix 秒转成 <input type="datetime-local"> 需要的本地字符串。 */
export function toLocalInput(unixSec: number | null | undefined): string {
  if (!unixSec) return ''
  const d = new Date(unixSec * 1000)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`
}
