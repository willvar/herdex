// Use native clipboard APIs first; legacy copy also works over ordinary LAN HTTP.
export async function copyText(text) {
  try {
    if (window.isSecureContext && navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text)
      return true
    }
  } catch { /* permission denied: try the selected-text path */ }
  const previous = document.activeElement
  const input = document.createElement('textarea')
  input.value = text
  input.readOnly = true
  input.style.cssText = 'position:fixed;left:-9999px;top:0'
  document.body.appendChild(input)
  input.select()
  try { return document.execCommand?.('copy') === true }
  catch { return false }
  finally { input.remove(); previous?.focus?.() }
}
