/** Complete the real name step before suites that focus on media or server behavior. */
export async function chooseFirstRunName(page, name) {
  await page.locator('main input[autocomplete="nickname"], input[name="server-address"]').first().waitFor();
  const field = page.locator('main input[autocomplete="nickname"]');
  if (await field.isVisible()) {
    await field.fill(name);
    await page.locator("main form").getByRole("button", { name: "Continue", exact: true }).click();
  }
  await page.getByLabel("Server address").waitFor();
}
