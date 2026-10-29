import { DEFAULT_INPUT_DB, InputGate } from "./input-sensitivity";

declare class AudioWorkletProcessor { readonly port: MessagePort; }
declare function registerProcessor(name: string, processor: new () => AudioWorkletProcessor): void;
declare const currentTime: number;

export interface GateControls { threshold: number; ptt: boolean | null; blocked: boolean; automatic?: boolean }

class InputGateProcessor extends AudioWorkletProcessor {
  private gate = new InputGate();
  private controls: GateControls = { threshold: DEFAULT_INPUT_DB, ptt: null, blocked: true };

  constructor() {
    super();
    this.port.onmessage = (event: MessageEvent<GateControls>) => { this.controls = event.data; };
  }

  process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    // Manual sensitivity measures capture as the settings meter does. Automatic
    // detection keeps the legacy web gate's position AFTER gain and suppression:
    // gain can lift a quiet voice, and suppressed noise must not keep it open.
    const audio = inputs[1]?.[0];
    const measured = this.controls.automatic ? audio : inputs[0]?.[0];
    const output = outputs[0]?.[0];
    if (!output) return true;
    let power = 0;
    if (measured) for (const sample of measured) power += sample * sample;
    const db = measured?.length ? 10 * Math.log10(power / measured.length) : -Infinity;
    const { threshold, ptt, blocked } = this.controls;
    const send = this.gate.allows(db, currentTime * 1000, threshold, ptt, blocked);
    output.fill(0);
    if (send && audio) output.set(audio);
    return true;
  }
}

registerProcessor("gwar-input-gate", InputGateProcessor);
