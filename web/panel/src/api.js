// Manage API helper — shared manage-key gate + JSON fetch with error unwrap.
let MK = localStorage.getItem('herdex_mk') || '';

export function getKey() { return MK; }
export function setKey(k) {
  MK = k || '';
  localStorage.setItem('herdex_mk', MK);
}

export async function api(method, path, body) {
  const res = await fetch('/manage/api' + path, {
    method,
    headers: Object.assign(
      { 'Content-Type': 'application/json' },
      MK ? { Authorization: `Bearer ${MK}` } : {},
    ),
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let data;
  try { data = text ? JSON.parse(text) : null; } catch { data = null; }
  if (!res.ok) {
    throw new Error((data && (data.error || data.message)) || text.slice(0, 120) || `HTTP ${res.status}`);
  }
  return data;
}

export function fmtTok(n) {
  n = n || 0;
  if (n >= 1e9) return (n / 1e9).toFixed(2) + 'B';
  if (n >= 1e6) return (n / 1e6).toFixed(2) + 'M';
  if (n >= 1e3) return (n / 1e3).toFixed(1) + 'k';
  return String(n);
}
