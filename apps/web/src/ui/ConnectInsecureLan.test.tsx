// Gate regressions (Astra r6 probes): the plaintext (insecure LAN) choice belongs to one saved address only.
// @vitest-environment happy-dom
import { act, type ComponentProps } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { beforeEach, afterEach, it, expect, vi } from 'vitest';
import { ConnectScreen } from './ConnectScreen';
import { TooltipProvider } from './kit';
import { useSettings } from '../state/settings';
import { useAccount } from '../state/account';
import { useConnectUi } from '../state/stores';
const mock = vi.hoisted(() => ({ connect: vi.fn(), inputs: new Map<string, (s: string) => void>() }));
vi.mock('../state/controller', () => ({ controller: { connectInteractive: mock.connect }, describeBan: () => '' }));
vi.mock('../platform', async (get) => ({ ...await get<typeof import('../platform')>(), isDesktop: () => true }));
vi.mock('./hooks', async (get) => ({ ...await get<typeof import('./hooks')>(), useTsIdentities: () => null }));
vi.mock('./kit', async (get) => {
  const original = await get<typeof import('./kit')>();
  return { ...original, Input: (props: ComponentProps<typeof original.Input>) => {
    if (props.value) mock.inputs.set(String(props.value), (value) => props.onChange?.({ target: { value } } as never));
    return <original.Input {...props} />;
  } };
});
let root: Root, div: HTMLDivElement;
const submit = async () => act(async () => { div.querySelector('form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); });
const change = async (from: string, to: string) => act(async () => mock.inputs.get(from)!(to));
const show = async () => act(async () => { root.render(<TooltipProvider><ConnectScreen /></TooltipProvider>); });
const choice = () => mock.connect.mock.calls.at(-1)![0];
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true); mock.connect.mockReset(); mock.inputs.clear();
  useAccount.setState({ loaded: true, account: null });
  useSettings.setState({ onboarding: { completed: true, firstUsedAt: 1, nudgeDismissed: false, roleReceived: false }, language: 'en', lastAddress: 'a.lan', lastKind: 'gwar', bookmarks: [{ id: 'a', name: 'A', address: 'a.lan', kind: 'gwar', updatedAt: 1, insecureLan: true }] });
  useConnectUi.setState({ busy: false, error: null, needPassword: false, invite: null });
  div = document.createElement('div'); document.body.appendChild(div); root = createRoot(div);
});
afterEach(async () => { await act(async () => root.unmount()); div.remove(); vi.unstubAllGlobals(); });
it('never carries the saved plaintext choice to a typed unbookmarked address', async () => {
  await show(); await submit(); expect(choice()).toMatchObject({ address: 'a.lan', insecureLan: true });
  await change('a.lan', 'b.lan'); await submit(); expect(choice()).toMatchObject({ address: 'b.lan', insecureLan: false });
});
it('new invite address defaults to TLS', async () => {
  await show(); await act(async () => useConnectUi.setState({ invite: { server: 'b.lan', code: 'abc', serverKey: 'pinned-key' } }));
  await submit(); expect(choice()).toMatchObject({ address: 'b.lan', insecureLan: false });
});
it('bookmark removal withdraws plaintext for the current address', async () => {
  await show(); await act(async () => useSettings.getState().removeBookmark('a'));
  await submit(); expect(choice()).toMatchObject({ address: 'a.lan', insecureLan: false });
});
it('uses only the new address own saved plaintext setting', async () => {
  await show(); await act(async () => useSettings.getState().saveBookmark({ id: 'b', name: 'B', address: 'b.lan', kind: 'gwar', insecureLan: true }));
  await change('a.lan', 'b.lan'); await submit(); expect(choice()).toMatchObject({ address: 'b.lan', insecureLan: true });
  await act(async () => useSettings.getState().saveBookmark({ id: 'b', name: 'B', address: 'b.lan', kind: 'gwar', insecureLan: false }));
  await submit(); expect(choice()).toMatchObject({ address: 'b.lan', insecureLan: false });
});

it('keeps TeamSpeak reachable behind Advanced and the public option always secure', async () => {
  useSettings.setState({ lastAddress: '', bookmarks: [], lastKind: 'gwar' });
  await show();
  const disclosure = div.querySelector('details')!;
  expect(disclosure.open).toBe(false);
  expect(disclosure.querySelector('[role="radio"]')?.textContent).toBe('Gwar server');
  await act(async () => (Array.from(div.querySelectorAll('button')).find(b => b.textContent === 'Join the public server (gwar.app)')!).click());
  expect(choice()).toMatchObject({ kind: 'gwar', address: 'gwar.app', insecureLan: false });
  await act(async () => disclosure.querySelector('summary')!.click());
  await act(async () => (Array.from(disclosure.querySelectorAll<HTMLButtonElement>('[role="radio"]')).find(b => b.textContent === 'TeamSpeak server')!).click());
  expect(disclosure.open).toBe(true);
});

it('leaves Advanced open when switching back from TeamSpeak or turning off insecure LAN', async () => {
  await show();
  const disclosure = div.querySelector('details')!;
  expect(disclosure.open).toBe(true);
  const radio = (name: string) => Array.from(disclosure.querySelectorAll<HTMLButtonElement>('[role="radio"]')).find(b => b.textContent === name)!;
  await act(async () => radio('TeamSpeak server').click());
  await act(async () => radio('Gwar server').click());
  expect(disclosure.open).toBe(true);
  const toggle = disclosure.querySelector<HTMLButtonElement>('[role="switch"]')!;
  await act(async () => toggle.click());
  expect(disclosure.open).toBe(true);
  await act(async () => disclosure.querySelector('summary')!.click());
  await act(async () => useConnectUi.setState({ error: 'A failed connection does not reopen this section.' }));
  expect(disclosure.open).toBe(false);
});
