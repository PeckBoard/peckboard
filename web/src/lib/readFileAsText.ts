/**
 * Read a picked file into text, so a "choose a file" affordance and a paste
 * field end at the same string. Used by every PEM-shaped input (the TLS
 * certificate/key upload and the SSH key vault's import dialog): a private
 * key is multi-line, and a single-line `<input>` silently eats the newlines.
 */
export function readFileAsText(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve(String(reader.result ?? ''))
    reader.onerror = () => reject(reader.error ?? new Error('read failed'))
    reader.readAsText(file)
  })
}
