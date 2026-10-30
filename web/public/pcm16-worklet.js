/**
 * AudioWorklet for speech-server recognition (src/voice/serverStt.ts):
 * downsamples the microphone to 16 kHz mono signed 16-bit PCM and posts
 * one transferable `ArrayBuffer` per frame (`frameSamples`, 100 ms by
 * default). A same-origin file rather than a Blob URL: the CSP's
 * `script-src 'self'` governs worklet modules.
 *
 * Box-filter decimation (average the input samples behind each output
 * sample) — cheap, and enough anti-aliasing for speech recognition.
 */
class Pcm16Downsampler extends AudioWorkletProcessor {
  constructor(options) {
    super()
    const o = (options && options.processorOptions) || {}
    this.outRate = o.outRate || 16000
    this.frameSamples = o.frameSamples || 1600
    this.ratio = Math.max(1, sampleRate / this.outRate)
    this.buf = new Int16Array(this.frameSamples)
    this.n = 0
    this.acc = 0
    this.accN = 0
    this.pos = 0
  }

  process(inputs) {
    const ch = inputs[0] && inputs[0][0]
    if (!ch) return true
    for (let i = 0; i < ch.length; i++) {
      this.acc += ch[i]
      this.accN++
      this.pos += 1
      if (this.pos < this.ratio) continue
      this.pos -= this.ratio
      const s = this.acc / this.accN
      this.acc = 0
      this.accN = 0
      this.buf[this.n++] = Math.max(-32768, Math.min(32767, Math.round(s * 32767)))
      if (this.n === this.frameSamples) {
        this.port.postMessage(this.buf.buffer, [this.buf.buffer])
        this.buf = new Int16Array(this.frameSamples)
        this.n = 0
      }
    }
    return true
  }
}

registerProcessor('pcm16-downsampler', Pcm16Downsampler)
