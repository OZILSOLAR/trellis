import { expect, test } from '@playwright/test';
import { installFreighterMock } from './freighter-mock';

/**
 * Create-to-resolve journey (#499): Create Agreement -> Lock Funds -> Submit
 * Work -> Approve & Release, driven through the real UI with a mocked
 * Freighter wallet against the local devnet started by tests/e2e/run.sh.
 * The final assertion reads the milestone status back from chain via the UI.
 */
test('create, fund, submit work and approve release through the UI', async ({ page }) => {
  const env = {
    payer: process.env.E2E_PAYER_ADDRESS,
    payerSecret: process.env.E2E_PAYER_SECRET,
    payee: process.env.E2E_PAYEE_ADDRESS,
    payeeSecret: process.env.E2E_PAYEE_SECRET,
    resolver: process.env.E2E_RESOLVER_ADDRESS,
    token: process.env.E2E_TOKEN_ADDRESS,
    rpcUrl: process.env.E2E_RPC_URL,
    passphrase: process.env.E2E_NETWORK_PASSPHRASE,
  };
  for (const [name, value] of Object.entries(env)) {
    expect(value, `E2E env var for ${name} must be set — see tests/e2e/run.sh`).toBeTruthy();
  }

  const wallet = await installFreighterMock(page, {
    address: env.payer!,
    secret: env.payerSecret!,
    networkPassphrase: env.passphrase!,
    rpcUrl: env.rpcUrl!,
  });

  // Distinct from create-agreement.spec.ts so both can run against one devnet.
  const agreementId = (Date.now() + 1).toString(16).padStart(64, '0');

  await page.goto('/');
  await page.getByRole('button', { name: 'Connect Wallet' }).click();
  await expect(page.getByText(env.payer!.slice(0, 4))).toBeVisible({ timeout: 15_000 });

  // 1. Create Agreement (payer)
  await page.goto('/create');
  await page.getByLabel('Agreement ID (hex)').fill(agreementId);
  await page.getByLabel('Payee Address').fill(env.payee!);
  await page.getByLabel('Token Contract Address').fill(env.token!);
  await page.getByLabel('Dispute Resolver Address').fill(env.resolver!);
  await page.getByPlaceholder('Amount (smallest token unit)').fill('1000');
  await page.getByRole('button', { name: 'Create Agreement' }).click();
  await expect(page).toHaveURL(new RegExp(`/agreement/${agreementId}$`), { timeout: 30_000 });

  // 2. Lock Funds (payer)
  await page.getByRole('button', { name: 'Lock Funds' }).first().click();
  await page.getByRole('button', { name: 'Confirm' }).click();
  await expect(page.getByText('Funded').first()).toBeVisible({ timeout: 30_000 });

  // 3. Submit Work (payee) — swap the mocked wallet identity and reload.
  await wallet.switchAccount(env.payee!, env.payeeSecret!);
  await page.reload();
  await page.getByRole('button', { name: 'Connect Wallet' }).click();
  await page.getByRole('button', { name: 'Submit Work' }).first().click();
  await page.getByPlaceholder(/Proof URI/).fill('ipfs://e2e-proof');
  await page.getByRole('button', { name: 'Submit', exact: true }).click();
  await expect(page.getByText('WorkSubmitted').first()).toBeVisible({ timeout: 30_000 });

  // 4. Approve & Release (payer)
  await wallet.switchAccount(env.payer!, env.payerSecret!);
  await page.reload();
  await page.getByRole('button', { name: 'Connect Wallet' }).click();
  await page.getByRole('button', { name: 'Approve & Release' }).first().click();
  await page.getByRole('button', { name: 'Confirm' }).click();
  await expect(page.getByText('Completed').first()).toBeVisible({ timeout: 30_000 });
});
