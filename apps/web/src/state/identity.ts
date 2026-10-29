import { create } from "zustand";

/**
 * The current identity profile, including changes picked up from the account vault. `replaced` counts the times
 * another identity took its place (signing in or out, an import): what was kept for the one before goes with it.
 */
export const useAccountIdentity = create<{ nickname: string; replaced: number }>()(() => ({ nickname: "", replaced: 0 }));
