async function measure() {
  const { WebRtcVoiceEngine } = await window.__gwarE2eImport("/src/voice/webrtc-engine.ts");
  const { denoiseModels, denoisePreparation, prepareDenoise } = await window.__gwarE2eImport("/src/voice/denoise/index.ts");
  const { useSettings } = await window.__gwarE2eImport("/src/state/settings.ts");
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  const wait = async (predicate, what) => {
    const started = performance.now();
    while (!predicate()) { if (performance.now() - started > 30_000) throw new Error(`timed out: ${what}; ${JSON.stringify({ model: denoiseModels?.status(), pipeline: denoisePreparation?.status(), workers: window.__denoiseWorkers.created })}`); await sleep(20); }
  };
  const longTasks = [];
  const supported = PerformanceObserver.supportedEntryTypes?.includes("longtask") === true;
  const observer = supported ? new PerformanceObserver(list => {
    for (const task of list.getEntries()) longTasks.push({ at: task.startTime, ms: task.duration });
  }) : null;
  observer?.observe({ type: "longtask" });
  const engine = new WebRtcVoiceEngine();
  const failures = [];
  const joins = [];
  const prebuilt = window.__denoiseCase === "warm";
  const idleStarted = performance.now();
  useSettings.setState({ audio: { ...useSettings.getState().audio, noiseSuppression: true } });
  // The real login subscription has separate controller coverage. Isolate its idle hook
  // without fabricating an authenticated UI or including server RTT in the media metric.
  const prepare = () => {
    if (prepareDenoise) prepareDenoise(true, true);
    else denoiseModels?.configure(true, true);
  };
  if (prebuilt) {
    prepare();
    if (denoiseModels) await wait(() => denoiseModels.status().state === "ready", "idle snapshot");
    if (denoisePreparation) await wait(() => denoisePreparation.status().state === "ready", "idle warmed pipeline");
    // Both variants get the same idle opportunity after the login state transition.
    await sleep(Math.max(0, 5000 - (performance.now() - idleStarted)));
  }
  const idleFinished = performance.now();
  const idleStatus = denoiseModels?.status() ?? null;
  const idlePipeline = denoisePreparation?.status() ?? null;
  const beforeJoins = { created: window.__denoiseWorkers.created, active: window.__denoiseWorkers.active.size };
  try {
    for (let join = 0; join < 2; join++) {
      const phases = [];
      let settle;
      const attached = new Promise(resolve => { settle = resolve; });
      await window.__denoiseJoinGesture();
      const started = performance.now();
      if (!prebuilt && join === 0) prepare();
      window.__gwarDenoiseCallMark = stage => {
        phases.push({ stage, ms: performance.now() - started });
        if (stage === "source-ready") settle();
      };
      // No remote transport is opened: use the real engine/capture/gate/model initialization,
      // then leave its offer pending until stop. The quality suite runs independently.
      void engine.start({
        mode: "plaintext", iceServers: [], offer: () => new Promise(() => {}), inputGate: async () => {},
        binding: { server_key: null, session: 1, channel: 1, call: null, participation: null, group: null },
      }).catch(error => { failures.push(String(error)); settle(); });
      let timer;
      try {
        await Promise.race([attached, new Promise((_, reject) => {
          timer = setTimeout(() => reject(new Error(`call suppression readiness timed out: ${JSON.stringify({ phases, failures, model: denoiseModels?.status(), pipeline: denoisePreparation?.status() })}`)), 30_000);
        })]);
        const readyMs = performance.now() - started;
        if (failures.length || !phases.some(p => p.stage === "source-ready")) throw new Error(JSON.stringify({ failures, phases }));
        joins.push({ join, readyMs, phases: [...phases], model: denoiseModels?.status() ?? null });
        if (join === 1) {
          await engine.setInputDevice("default");
          await engine.setInputDevice(null);
          if (denoiseModels && denoiseModels.status().builds !== 1) throw new Error("device changes rebuilt the model");
        }
      } finally { clearTimeout(timer); await engine.stop(); delete window.__gwarDenoiseCallMark; }
    }
    const retained = denoiseModels?.status() ?? null;
    if (denoisePreparation) await wait(() => denoisePreparation.status().state === "ready", "returned idle pipeline");
    const retainedPipeline = denoisePreparation?.status() ?? null;
    if (denoiseModels && (retained.builds !== 1 || retained.bytes === 0)) throw new Error("joins did not reuse one cached snapshot");
    useSettings.setState({ audio: { ...useSettings.getState().audio, noiseSuppression: false } });
    await sleep(100);
    const released = denoiseModels?.status() ?? null;
    const releasedPipeline = denoisePreparation?.status() ?? null;
    if (denoiseModels && (released.bytes !== 0 || window.__denoiseWorkers.active.size !== 0)) throw new Error("disabled suppression retained its model worker");
    if (denoisePreparation && (retainedPipeline.pipelines !== 1 || retainedPipeline.idleRunning || releasedPipeline.pipelines !== 0)) throw new Error("prepared runtime was not suspended/released");
    const idleLongTasks = longTasks.filter(task => task.at >= idleStarted && task.at < idleFinished);
    const result = {
      type: "DENOISE_JOIN", scenario: prebuilt ? "warm" : "cold", hasCache: !!denoiseModels,
      joins, idleStatus, idlePipeline, retainedPipeline, releasedPipeline, idleMs: idleFinished - idleStarted, idleLongTasks,
      longTasksSupported: supported, longTasks, beforeJoins, retained, released,
      workers: { created: window.__denoiseWorkers.created, active: window.__denoiseWorkers.active.size }, failures,
    };
    console.log(JSON.stringify(result));
    if (denoiseModels && prebuilt && supported && idleLongTasks.length) throw new Error(`idle prebuild blocked the page: ${JSON.stringify(idleLongTasks)}`);
    return result;
  } finally { observer?.disconnect(); await engine.stop(); delete window.__gwarDenoiseCallMark; }
}
