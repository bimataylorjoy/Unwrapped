import { describe, expect, it } from "vitest";
import { addressToScript, estimateVsize, formatBtc, merkleProof, parseBtc, planWithdrawal } from "../src/btc.ts";
import { formatUsdc, parseOutcome, parseUsdc } from "../src/contract.ts";

describe("merkleProof", () => {
  // Bitcoin mainnet block 100000: four transactions and a well-known root.
  const txids = [
    "8c14f0db3df150123e6f3dbbf30f8b955a8249b62ac1d1ff16284aefa3d06d87",
    "fff2525b8931402dd09222c50775608f75787bd2b87e56995a7bdd30f79702c4",
    "6359f0868171b1d194cbee1af2f16ea598ae8fad666d9b012c8ed2b79a236ec4",
    "e9a66845e05d5abc0ad04ec80f774a7e585c6e8db975962d069a522137b80c1d",
  ];
  const root = "f3e94742aca4b5ef85488dc37c06c3282295ffec960994b2c0d5ac2a25a95766";

  it("reproduces the merkle root of block 100000", () => {
    for (let i = 0; i < txids.length; i++) {
      const p = merkleProof(txids, i);
      expect(p.root).toBe(root);
      expect(p.proof).toHaveLength(2);
    }
    expect(merkleProof(txids, 0).proof[0]).toBe(txids[1]);
  });

  it("pairs the last node with itself on odd levels", () => {
    const three = txids.slice(0, 3);
    expect(merkleProof(three, 2).proof[0]).toBe(three[2]);
    expect(merkleProof(three, 2).root).toBe(merkleProof(three, 0).root);
    expect(() => merkleProof(three, 3)).toThrow();
  });
});

describe("addressToScript", () => {
  it("handles every standard address type", () => {
    // BIP173 / BIP350 vectors.
    expect(addressToScript("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4", "mainnet")).toBe(
      "0014751e76e8199196d454941c45d1b3a323f1433bd6",
    );
    expect(addressToScript("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx", "testnet")).toBe(
      "0014751e76e8199196d454941c45d1b3a323f1433bd6",
    );
    expect(addressToScript("tb1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3q0sl5k7", "testnet")).toBe(
      "00201863143c14c5166804bd19203356da136c985678cd4d27a1b8c6329604903262",
    );
    expect(addressToScript("bc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqzk5jj0", "mainnet")).toBe(
      "512079be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
    );
    expect(addressToScript("1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2", "mainnet")).toBe(
      "76a91477bff20c60e522dfaa3350c39b030a5d004e839a88ac",
    );
    expect(addressToScript("3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy", "mainnet")).toBe(
      "a914b472a266d0bd89c13706a4132ccfb16f7c3b9fcb87",
    );
  });

  it("rejects the wrong network and bad checksums", () => {
    expect(() => addressToScript("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4", "testnet")).toThrow();
    expect(() => addressToScript("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsy", "testnet")).toThrow();
    expect(() => addressToScript("1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN3", "mainnet")).toThrow();
    expect(() => addressToScript("hello", "mainnet")).toThrow();
  });
});

describe("planWithdrawal", () => {
  const dest = "0014" + "11".repeat(20);
  const change = "0014" + "22".repeat(20);
  const utxos = [
    { txid: "aa".repeat(32), vout: 0, value: 30_000 },
    { txid: "bb".repeat(32), vout: 1, value: 100_000 },
  ];

  it("spends the largest output first and returns change to the vault", () => {
    const plan = planWithdrawal(utxos, dest, 50_000, change, 2);
    expect(plan.inputs).toEqual([{ txid: "bb".repeat(32), vout: 1, value: "100000" }]);
    expect(plan.outputs[0]).toEqual({ script_pubkey: dest, value: "50000" });
    expect(plan.outputs[1]!.script_pubkey).toBe(change);
    expect(plan.vsize).toBe(estimateVsize(1, [dest, change]));
    expect(50_000 + Number(plan.outputs[1]!.value) + plan.feeSats).toBe(100_000);
    expect(plan.feeSats).toBe(plan.vsize * 2);
  });

  it("adds inputs until the amount is covered", () => {
    const plan = planWithdrawal(utxos, dest, 120_000, change, 1);
    expect(plan.inputs).toHaveLength(2);
    const out = plan.outputs.reduce((s, o) => s + Number(o.value), 0);
    expect(out + plan.feeSats).toBe(130_000);
  });

  it("sweeps everything with max", () => {
    const plan = planWithdrawal(utxos, dest, "max", change, 3);
    expect(plan.inputs).toHaveLength(2);
    expect(plan.outputs).toHaveLength(1);
    expect(Number(plan.outputs[0]!.value) + plan.feeSats).toBe(130_000);
  });

  it("refuses what the vault cannot pay for", () => {
    expect(() => planWithdrawal(utxos, dest, 130_000, change, 1)).toThrow(/not enough/);
    expect(() => planWithdrawal([], dest, 1_000, change, 1)).toThrow(/no spendable/);
    expect(() => planWithdrawal(utxos, dest, 100, change, 1)).toThrow(/at least/);
  });
});

describe("formatting", () => {
  it("round-trips BTC and USDC amounts", () => {
    expect(formatBtc(123_456_789)).toBe("1.23456789");
    expect(parseBtc("0.001")).toBe(100_000);
    expect(() => parseBtc("0.123456789")).toThrow();
    expect(formatUsdc("1234567890")).toBe("1,234.56");
    expect(parseUsdc("1,234.5")).toBe(1_234_500_000n);
    expect(() => parseUsdc("1.2345678")).toThrow();
  });
});

describe("parseOutcome", () => {
  it("decodes the return value and surfaces a nested panic", () => {
    const ok = parseOutcome({
      status: { SuccessValue: btoa(JSON.stringify({ vault_id: 1 })) },
      transaction_outcome: { id: "abc", outcome: { logs: [], status: { SuccessReceiptId: "x" } } },
      receipts_outcome: [{ outcome: { logs: ["hello"], status: { SuccessValue: "" } } }],
    });
    expect(ok).toMatchObject({ ok: true, value: { vault_id: 1 }, logs: ["hello"], txHash: "abc" });

    const failed = parseOutcome({
      status: { SuccessValue: btoa("\"0\"") },
      receipts_outcome: [
        {
          outcome: {
            logs: [],
            status: { Failure: { ActionError: { index: 0, kind: { FunctionCallError: { ExecutionError: "Smart contract panicked: vault is healthy" } } } } },
          },
        },
      ],
    });
    expect(failed.ok).toBe(false);
    expect(failed.error).toBe("vault is healthy");
  });
});
