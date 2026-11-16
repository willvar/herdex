import { afterEach, describe, expect, it, vi } from 'vitest'
import { copyText } from '../clipboard.js'
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); document.body.innerHTML = '' })
describe('Clipboard platform integration', () => {
  it('supports plain HTTP with selected-text copy and removes temporary DOM', async () => {
    vi.stubGlobal('isSecureContext', false)
    document.execCommand = vi.fn().mockReturnValue(true)
    expect(await copyText('fixture text')).toBe(true)
    expect(document.execCommand).toHaveBeenCalledWith('copy')
    expect(document.querySelector('textarea')).toBeNull()
  })
  it('does not report copy success when the platform returns false', async () => {
    document.execCommand = vi.fn().mockReturnValue(false)
    expect(await copyText('fixture text')).toBe(false)
    expect(document.querySelector('textarea')).toBeNull()
  })
})
