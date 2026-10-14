/** A leader exiting does not mean its process group is gone (pnpm -> vite). */
export async function stopProcesses(children, graceMs = 5000) {
  const signal = (child, name) => {
    try { process.kill(-child.pid, name); return true; }
    catch (error) { if (error.code === "ESRCH") return false; throw error; }
  };
  for (const child of children) signal(child, "SIGTERM");
  const deadline = Date.now() + graceMs;
  while (children.some(child => signal(child, 0))) {
    if (Date.now() >= deadline) for (const child of children) signal(child, "SIGKILL");
    if (Date.now() >= deadline + 5000) throw new Error("Local test process group did not exit after SIGKILL");
    await new Promise(resolve => setTimeout(resolve, 50));
  }
}
