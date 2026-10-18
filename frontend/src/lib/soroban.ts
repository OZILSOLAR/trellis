import { Account, Contract, TransactionBuilder, nativeToScVal, scValToNative, rpc, xdr } from '@stellar/stellar-sdk';
import { CONTRACT_ID, RPC_URL, NETWORK_PASSPHRASE } from './config';

export interface Agreement {
  agreement_id: string;
  payer: string;
  payee: string;
  token: string;
  dispute_resolver: string;
  milestones: Milestone[];
}

export interface Milestone {
  id?: number;
  amount: string;
  status: EscrowStatus;
  proof_uri: string | null;
  /**
   * Cumulative amount already paid out via `release_partial`, decoded from the
   * agreement's `released_amounts` map. Absent / `'0'` when nothing has been
   * released. Use {@link remainingAmount} for what is still escrowed.
   */
  released?: string;
}

/** Parses an integer base-unit amount, treating missing or malformed input as zero. */
export function toBaseUnits(value: string | undefined | null): bigint {
  try {
    return value ? BigInt(value) : 0n;
  } catch {
    return 0n;
  }
}

/**
 * Amount still held in escrow for a milestone: its configured `amount` minus
 * whatever `release_partial` has already paid out. Mirrors the contract's
 * `remaining_amount` helper, in base units.
 */
export function remainingAmount(milestone: Pick<Milestone, 'amount' | 'released'>): bigint {
  const remaining = toBaseUnits(milestone.amount) - toBaseUnits(milestone.released);
  return remaining > 0n ? remaining : 0n;
}

/**
 * Looks up one milestone's cumulative release in a decoded `released_amounts`
 * (`Map<u32, i128>`). `scValToNative` turns a Soroban map into a plain object
 * keyed by the stringified milestone index, but a JS `Map` is accepted too.
 */
export function releasedAmountFor(releasedAmounts: unknown, milestoneId: number): string {
  let value: unknown;
  if (releasedAmounts instanceof Map) {
    value = releasedAmounts.get(milestoneId) ?? releasedAmounts.get(String(milestoneId));
  } else if (typeof releasedAmounts === 'object' && releasedAmounts !== null) {
    value = (releasedAmounts as Record<string, unknown>)[String(milestoneId)];
  }
  if (typeof value === 'bigint' || typeof value === 'number' || typeof value === 'string') {
    return value.toString();
  }
  return '0';
}

export type EscrowStatus =
  | 'Pending'
  | 'Funded'
  | 'WorkSubmitted'
  | 'Completed'
  | 'Disputed'
  | 'Refunded'
  | 'Cancelled';

export interface SorobanEvent {
  type: string;
  ledger: number;
  txHash: string;
  timestamp: number;
  data: Record<string, unknown>;
}

// RPC Response types for type-safe parsing
export interface RawMilestone {
  amount?: { toString: () => string } | string;
  status?: unknown;
  proof_uri?: string;
}

export interface RawAgreementResponse {
  payer?: string;
  payee?: string;
  token?: string;
  dispute_resolver?: string;
  milestones?: RawMilestone[];
  released_amounts?: unknown;
}

export interface RawEventResponse {
  ledger?: number;
  txHash?: string;
  ledgerClosedAt?: string;
  topic?: string[];
  value?: { xdr?: string };
}

// Type guards and validators
export function isValidAgreementResponse(data: unknown): data is RawAgreementResponse {
  if (typeof data !== 'object' || data === null) {
    return false;
  }
  const obj = data as Record<string, unknown>;
  return (
    (typeof obj.payer === 'string' || obj.payer === undefined) &&
    (typeof obj.payee === 'string' || obj.payee === undefined) &&
    (typeof obj.token === 'string' || obj.token === undefined) &&
    (typeof obj.dispute_resolver === 'string' || obj.dispute_resolver === undefined) &&
    (Array.isArray(obj.milestones) || obj.milestones === undefined)
  );
}

export function isValidEventResponse(data: unknown): data is RawEventResponse {
  if (typeof data !== 'object' || data === null) {
    return false;
  }
  const obj = data as Record<string, unknown>;
  return (
    (typeof obj.ledger === 'number' || obj.ledger === undefined) &&
    (typeof obj.txHash === 'string' || obj.txHash === undefined) &&
    (typeof obj.ledgerClosedAt === 'string' || obj.ledgerClosedAt === undefined) &&
    (Array.isArray(obj.topic) || obj.topic === undefined)
  );
}

export function parseEscrowStatus(statusValue: unknown): EscrowStatus {
  if (typeof statusValue === 'string') {
    const validStatuses: EscrowStatus[] = ['Pending', 'Funded', 'WorkSubmitted', 'Completed', 'Disputed', 'Refunded', 'Cancelled'];
    if (validStatuses.includes(statusValue as EscrowStatus)) {
      return statusValue as EscrowStatus;
    }
  }

  if (typeof statusValue === 'object' && statusValue !== null) {
    const keys = Object.keys(statusValue);
    if (keys.length > 0 && typeof keys[0] === 'string') {
      const status = keys[0];
      const validStatuses: EscrowStatus[] = ['Pending', 'Funded', 'WorkSubmitted', 'Completed', 'Disputed', 'Refunded', 'Cancelled'];
      if (validStatuses.includes(status as EscrowStatus)) {
        return status as EscrowStatus;
      }
    }
  }

  return 'Pending';
}

export const sorobanServer = {
  getEvents: async (_options?: unknown) => {
    return { events: [] };
  },
};

// Soroban RPC requires a signed-looking source account even for a read-only
// simulation; this well-known all-zero-signature address never needs a real
// key since the transaction is simulated, never submitted.
const READ_ONLY_SOURCE = 'GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF';

function rpcServer(): rpc.Server {
  return new rpc.Server(RPC_URL, { allowHttp: RPC_URL.startsWith('http://') });
}

/** Converts a 64-char hex string (a `BytesN<32>` agreement ID) to an XDR ScVal. */
export function agreementIdToScVal(hex: string): xdr.ScVal {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.substr(i * 2, 2), 16);
  }
  return nativeToScVal(bytes, { type: 'bytes' });
}

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

/**
 * Encodes one milestone as the XDR map the contract's `Milestone` struct
 * expects: exactly `{ amount: i128, proof_uri: Option<String>, status: EscrowStatus }`,
 * keys in sorted order. The struct has no `id` (a milestone's id is its index
 * in the vector) and no description, so neither may appear as a map key or the
 * contract fails to deserialize the argument. `Option::None` is `Void`;
 * `Some(uri)` is the bare string.
 */
export function milestoneToScVal(m: { amount: bigint | string; status: EscrowStatus; proof_uri?: string | null }): xdr.ScVal {
  return xdr.ScVal.scvMap([
    new xdr.ScMapEntry({
      key: xdr.ScVal.scvSymbol('amount'),
      val: nativeToScVal(BigInt(m.amount), { type: 'i128' }),
    }),
    new xdr.ScMapEntry({
      key: xdr.ScVal.scvSymbol('proof_uri'),
      val: m.proof_uri ? nativeToScVal(m.proof_uri, { type: 'string' }) : xdr.ScVal.scvVoid(),
    }),
    new xdr.ScMapEntry({
      key: xdr.ScVal.scvSymbol('status'),
      val: xdr.ScVal.scvVec([xdr.ScVal.scvSymbol(m.status)]),
    }),
  ]);
}

interface RawMilestone {
  id: number;
  amount: bigint;
  status: [EscrowStatus];
  proof_uri: string | null;
}

interface RawAgreement {
  agreement_id: Uint8Array;
  payer: string;
  payee: string;
  token: string;
  dispute_resolver: string;
  milestones: RawMilestone[];
  released_amounts?: unknown;
}

async function simulateRead<T>(method: string, args: xdr.ScVal[]): Promise<T> {
  const server = rpcServer();
  const contract = new Contract(CONTRACT_ID);

  const tx = new TransactionBuilder(new Account(READ_ONLY_SOURCE, '0'), {
    fee: '100',
    networkPassphrase: NETWORK_PASSPHRASE,
  })
    .addOperation(contract.call(method, ...args))
    .setTimeout(30)
    .build();

  const simulated = await server.simulateTransaction(tx);

  if (rpc.Api.isSimulationError(simulated)) {
    throw new Error(simulated.error);
  }
  if (!simulated.result) {
    throw new Error(`No result returned for ${method}`);
  }

  return scValToNative(simulated.result.retval) as T;
}

/** Reads a full `Agreement` from the contract by its 64-char hex ID. */
export async function getAgreement(agreementId: string): Promise<Agreement> {
  const raw = await simulateRead<RawAgreement>('get_agreement', [agreementIdToScVal(agreementId)]);

  return {
    agreement_id: bytesToHex(raw.agreement_id),
    payer: raw.payer,
    payee: raw.payee,
    token: raw.token,
    dispute_resolver: raw.dispute_resolver,
    milestones: raw.milestones.map((m, index) => ({
      id: m.id ?? index,
      amount: m.amount.toString(),
      status: m.status[0],
      proof_uri: m.proof_uri ?? null,
      released: releasedAmountFor(raw.released_amounts, m.id ?? index),
    })),
  };
}

export async function getAgreementEvents(): Promise<SorobanEvent[]> {
  return [];
}
