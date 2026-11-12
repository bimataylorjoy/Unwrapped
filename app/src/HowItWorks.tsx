import { useState } from "react";
import type { OutpointState } from "@unwrapped/sdk";
import type { Ctx } from "./App.tsx";
import { formatUsdc } from "@unwrapped/sdk";
import { D, app, nearAccountUrl, pct, short } from "./chain.ts";

const VERDICT: Record<OutpointState, string> = {
  collateral: "Collateral: a proven deposit that no signature has ever been issued for.",
  signed: "Signed: the contract has requested a signature spending this output. It can never be collateral.",
  unknown: "Unknown: not proven to the contract and never signed for by this vault.",
};

export function HowItWorks({ ctx }: { ctx: Ctx }) {
  const { cfg, price, priceError, reports } = ctx;
  const sourceOf = (id: string) => D.oracles?.find((o) => o.account === id)?.source ?? "";
  const ago = (ns: string) => {
    const sec = Math.max(0, Math.round((Date.now() - Number(BigInt(ns) / 1_000_000n)) / 1000));
    return sec < 120 ? `${sec}s ago` : sec < 7200 ? `${Math.round(sec / 60)} min ago` : `${Math.round(sec / 3600)} h ago`;
  };
  const [vaultId, setVaultId] = useState("1");
  const [outpoint, setOutpoint] = useState("");
  const [verdict, setVerdict] = useState<string>();

  return (
    <section className="how">
      <h2>Three rules make native Bitcoin usable as collateral</h2>
      <ol className="rules">
        <li>
          <h3>Only the contract can sign</h3>
          <p>
            Each vault's Bitcoin key is derived by NEAR's MPC network from{" "}
            <code>
              ({D.contract}, "vault-N")
            </code>
            . Chain Signatures binds a key to the account that asks for it, so no user, operator or other contract can
            request a signature for a vault.
          </p>
        </li>
        <li>
          <h3>It signs only what it builds</h3>
          <p>
            A withdrawal is a list of inputs and outputs. The contract computes the Bitcoin signature hashes (BIP143)
            on chain and writes every input to a permanent <em>signature ledger</em> before the request leaves, so it
            knows every output it has ever authorised spending.
          </p>
        </li>
        <li>
          <h3>Collateral = proven deposits − the ledger</h3>
          <p>
            A deposit counts after an SPV proof against the Bitcoin light client on NEAR, and only if no signature for
            that output was ever issued. A signature obtained earlier and kept in a drawer cannot drain a vault after it
            is pledged, liquidated or sold.
          </p>
        </li>
      </ol>

      <div className="grid-2">
        <article className="card">
          <h3>What happens when you borrow</h3>
          <ol className="steps">
            <li>Mint a vault. You receive an NFT and a native Bitcoin address.</li>
            <li>Send BTC to it from any wallet. It stays on Bitcoin.</li>
            <li>
              After {cfg.min_confirmations} confirmations, prove the deposit: the contract checks the merkle proof
              against Bitcoin block headers stored on NEAR.
            </li>
            <li>Borrow USDC, up to {pct(cfg.max_ltv_bps, 0)} of the BTC's value. The vault locks.</li>
            <li>Repay to unlock, then withdraw: the MPC network signs a Bitcoin transaction the contract built.</li>
            <li>
              If the loan passes {pct(cfg.liq_threshold_bps, 0)} LTV, a liquidator repays it and receives the NFT, and
              with it the Bitcoin key.
            </li>
          </ol>
        </article>

        <article className="card">
          <h3>Check the signature ledger</h3>
          <p className="muted small">
            Before buying or liquidating a vault, anyone can check whether an output on its address is clean.
          </p>
          <form
            className="stack-form"
            onSubmit={async (e) => {
              e.preventDefault();
              const [txid, vout] = outpoint.trim().split(":");
              if (!txid || !/^[0-9a-fA-F]{64}$/.test(txid) || !/^\d+$/.test(vout ?? "") || !/^\d+$/.test(vaultId)) {
                setVerdict("Enter a vault number and an outpoint as <txid>:<output index>.");
                return;
              }
              try {
                setVerdict(VERDICT[await app.outpointState(Number(vaultId), txid.toLowerCase(), Number(vout))]);
              } catch (err) {
                setVerdict(err instanceof Error ? err.message : String(err));
              }
            }}
          >
            <div className="row-form">
              <div className="field narrow">
                <span className="unit">Vault #</span>
                <input value={vaultId} onChange={(e) => setVaultId(e.target.value)} inputMode="numeric" aria-label="Vault number" />
              </div>
              <div className="field">
                <input value={outpoint} onChange={(e) => setOutpoint(e.target.value)} placeholder="txid:vout" aria-label="Outpoint" />
              </div>
              <button className="btn" type="submit">
                Look up
              </button>
            </div>
            {verdict && <p className="notice">{verdict}</p>}
          </form>
        </article>
      </div>

      <article className="card">
        <h3>Where the BTC price comes from</h3>
        <p className="muted small">
          {cfg.oracle_ids.length} independent reporters each read a different exchange. The contract uses the median of
          the reports younger than {Math.round(cfg.max_price_age_sec / 60)} minutes, needs at least{" "}
          {cfg.min_oracle_reports} of them, and refuses any price when they disagree by more than{" "}
          {pct(cfg.max_oracle_spread_bps)}. No single reporter can set the price.
        </p>
        <div className="table-wrap flat">
          <table>
            <thead>
              <tr>
                <th>Reporter</th>
                <th>Source</th>
                <th className="r">Last report</th>
                <th className="r">When</th>
              </tr>
            </thead>
            <tbody>
              {cfg.oracle_ids.map((id) => {
                const r = reports.find((x) => x.oracle_id === id);
                return (
                  <tr key={id}>
                    <td>
                      <a href={nearAccountUrl(id)} target="_blank" rel="noreferrer" title={id}>
                        {short(id, 18, 8)}
                      </a>
                    </td>
                    <td>{sourceOf(id)}</td>
                    <td className="r num">{r ? `$${formatUsdc(r.usdc_per_btc)}` : "—"}</td>
                    <td className="r">
                      {r ? ago(r.updated_at_ns) : "never"}
                      {r && !r.fresh && <span className="status warn"> stale</span>}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
        <p className={`notice ${price ? "ok" : "error"}`}>
          {price
            ? `Price in use: $${formatUsdc(price.usdc_per_btc)}, median of ${price.sources} reports, spread ${pct(price.spread_bps, 2)}.`
            : `No price in use: ${priceError}. Borrowing, collateral withdrawals and liquidations are paused; repaying still works.`}
        </p>
      </article>

      <article className="card">
        <h3>What you are trusting</h3>
        <ul className="trust">
          <li>
            <strong>NEAR's MPC network</strong> (
            <a href={nearAccountUrl(cfg.mpc_signer)} target="_blank" rel="noreferrer">
              {cfg.mpc_signer}
            </a>
            ) to sign only when this contract asks. This replaces a bridge's custodian or multisig.
          </li>
          <li>
            <strong>The Bitcoin light client</strong> (
            <a href={nearAccountUrl(cfg.btc_light_client)} target="_blank" rel="noreferrer">
              {cfg.btc_light_client}
            </a>
            ) to follow Bitcoin's proof-of-work chain. Deposits need {cfg.min_confirmations} confirmations here.
          </li>
          <li>
            <strong>The price reporters</strong>: {cfg.min_oracle_reports} of {cfg.oracle_ids.length} must agree. On
            this testnet deployment all reporters are run by the project from one machine; independence comes from giving
            each reporter to a different operator. Pyth, the usual choice, no longer supports NEAR.
          </li>
          <li>
            <strong>The operator</strong> can change risk parameters and the list of reporters, and, on testnet,
            upgrade the contract code. The
            signer, light client, Bitcoin network and lent token cannot be changed after deployment.
          </li>
        </ul>
      </article>
    </section>
  );
}
