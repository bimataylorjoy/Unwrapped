import { formatBtc, formatUsdc, type Vault } from "@unwrapped/sdk";
import type { Ctx } from "./App.tsx";
import { app, btcAddressUrl, nearTxUrl, pct, short } from "./chain.ts";

export function MarketPanel({ ctx, vaults }: { ctx: Ctx; vaults: Vault[] }) {
  const { cfg, balances } = ctx;

  const liquidate = (v: Vault) =>
    ctx.run(`Liquidating vault #${v.vault_id}`, async () => {
      const quote = await app.liquidationQuote(v.vault_id);
      if (!quote) throw new Error(`No quote: there is no BTC price right now (${ctx.priceError ?? "reporters unavailable"}).`);
      if (!quote.liquidatable) throw new Error("This vault is healthy again.");
      // 1% headroom in case the price moves; the unused part is refunded.
      const amount = BigInt(quote.cost) + BigInt(quote.cost) / 100n;
      if (amount > balances.usdc) {
        throw new Error(`You need ${formatUsdc(amount)} USDC in your wallet; you have ${formatUsdc(balances.usdc)}.`);
      }
      const res = await ctx.send(app.liquidate(cfg.usdc, v.vault_id, amount));
      return {
        detail: `You bought ${formatBtc(quote.collateral_sats)} BTC for ${formatUsdc(quote.cost)} USDC. The vault and its Bitcoin key are yours.`,
        href: res.txHash && nearTxUrl(res.txHash),
      };
    });

  return (
    <section>
      <div className="section-head">
        <h2>All vaults</h2>
        <span className="muted small">
          Above {pct(cfg.liq_threshold_bps, 0)} LTV anyone can buy the vault at {pct(cfg.liq_bonus_bps, 0)} under the oracle
          price. The debt is repaid first; the rest goes to the borrower.
        </span>
      </div>
      {vaults.length === 0 ? (
        <p className="empty">No vaults yet.</p>
      ) : (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Vault</th>
                <th>Owner</th>
                <th>Bitcoin address</th>
                <th className="r">Collateral</th>
                <th className="r">Debt</th>
                <th className="r">LTV</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {vaults.map((v) => (
                <tr key={v.vault_id} className={v.liquidatable ? "danger" : ""}>
                  <td>#{v.vault_id}</td>
                  <td title={v.owner_id}>{short(v.owner_id, 12, 8)}</td>
                  <td>
                    <a href={btcAddressUrl(v.btc_address)} target="_blank" rel="noreferrer">
                      {short(v.btc_address, 9, 6)}
                    </a>
                  </td>
                  <td className="r num">{formatBtc(v.collateral_sats)} BTC</td>
                  <td className="r num">{formatUsdc(v.debt)}</td>
                  <td className="r num">{v.ltv_bps === null ? "—" : BigInt(v.debt) === 0n ? "—" : pct(Math.min(v.ltv_bps, 99_999))}</td>
                  <td className="r">
                    {v.liquidatable ? (
                      <button
                        className="btn small primary"
                        disabled={ctx.busy || !ctx.session || v.owner_id === ctx.session.accountId}
                        title={
                          !ctx.session
                            ? "Connect to liquidate"
                            : v.owner_id === ctx.session.accountId
                              ? "Repay your own vault instead"
                              : "Buy this vault at a discount"
                        }
                        onClick={() => liquidate(v)}
                      >
                        Liquidate
                      </button>
                    ) : (
                      <span className="status ok">{BigInt(v.debt) > 0n ? "Healthy" : "No debt"}</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
