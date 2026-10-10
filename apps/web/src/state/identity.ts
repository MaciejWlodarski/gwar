import { create } from "zustand";

/** The current identity profile, including changes picked up from the account vault. */
export const useAccountIdentity = create<{ nickname: string }>()(() => ({ nickname: "" }));
