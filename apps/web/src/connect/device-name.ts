/** A readable name for this device in the account's device list: "Chrome on macOS", "Gwar desktop on Windows". */

export function osName(userAgent: string): string {
  if (/Android/i.test(userAgent)) return "Android";
  if (/iPhone|iPad|iPod/i.test(userAgent)) return "iOS";
  if (/Windows/i.test(userAgent)) return "Windows";
  if (/Mac OS X|Macintosh/i.test(userAgent)) return "macOS";
  if (/CrOS/i.test(userAgent)) return "ChromeOS";
  if (/Linux|X11/i.test(userAgent)) return "Linux";
  return "unknown OS";
}

export function browserName(userAgent: string): string {
  if (/Edg(e|A|iOS)?\//.test(userAgent)) return "Edge";
  if (/OPR\/|Opera/.test(userAgent)) return "Opera";
  if (/Firefox\/|FxiOS\//.test(userAgent)) return "Firefox";
  if (/Chrome\/|CriOS\//.test(userAgent)) return "Chrome";
  if (/Safari\//.test(userAgent)) return "Safari";
  return "Browser";
}

export function deviceName(userAgent: string, desktop: boolean): string {
  return `${desktop ? "Gwar desktop" : browserName(userAgent)} on ${osName(userAgent)}`;
}
