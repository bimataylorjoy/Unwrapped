import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { formatUsdc, type Config, type Pool, type Price, type PriceReport, type TxCall, type TxResult, type Vault } from "@unwrapped/sdk";
import { D, app, nearAccountUrl, nearTxUrl, pct, short } from "./chain.ts";
import { connectDevKey, connectWallet, restoreSession, type Session } from "./wallet.ts";
import { VaultCard } from "./VaultCard.tsx";
import { LendPanel } from "./LendPanel.tsx";
import { MarketPanel } from "./MarketPanel.tsx";
import { HowItWorks } from "./HowItWorks.tsx";

export interface Balances {
  usdc: bigint;
  shares: bigint;
  supplied: bigint;
  claimable: bigint;
  registered: boolean;
}

export interface Toast {
  id: number;
  kind: "pending" | "ok" | "error";
  title: string;
  detail?: string;
  href?: string;
}

/** What an action reports back for its success toast. */
export interface Done {
  detail?: string;
  href?: string;
}

export interface Ctx {
  session?: Session;
  cfg: Config;
  /** Median of the reporters; null while there is no quorum. */
  price: Price | null;
  priceFresh: boolean;
  /** Why there is no price, when there is none. */
  priceError: string | null;
  reports: PriceReport[];
  pool: Pool;
  balances: Balances;
  busy: boolean;
  /** Runs an action with a progress toast, then reloads chain state. */
  run(title: string, action: () => Promise<Done | void>): Promise<void>;
  /** Sends a call as the connected account; throws with the panic message. */
  send(call: TxCall): Promise<TxResult>;
  connect(): void;
}

type Tab = "borrow" | "lend" | "market" | "how";

const EMPTY_BALANCES: Balances = { usdc: 0n, shares: 0n, supplied: 0n, claimable: 0n, registered: false };

export function App() {
  const [session, setSession] = useState<Session>();
  const [cfg, setCfg] = useState<Config>();
  const [price, setPrice] = useState<Price | null>(null);
  const [priceFresh, setPriceFresh] = useState(false);
  const [priceError, setPriceError] = useState<string | null>(null);
  const [reports, setReports] = useState<PriceReport[]>([]);
  const [pool, setPool] = useState<Pool>();
  const [myVaults, setMyVaults] = useState<Vault[]>([]);
  const [allVaults, setAllVaults] = useState<Vault[]>([]);
  const [balances, setBalances] = useState<Balances>(EMPTY_BALANCES);
  const [tab, setTab] = useState<Tab>("borrow");
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [busy, setBusy] = useState(false);
  const [connecting, setConnecting] = useState(false);
  const [loadError, setLoadError] = useState<string>();
  const toastId = useRef(0);
  const accountId = session?.accountId;

  const refresh = useCallback(async () => {
    try {
      const [c, p, why, rep, pl, all] = await Promise.all([
        app.config(),
        app.price(),
        app.priceError(),
        app.priceReports(),
        app.pool(),
        app.vaults(),
      ]);
      const fresh = p !== null;
      setPriceError(why);
      setReports(rep);
      setCfg(c);
      setPrice(p);
      setPriceFresh(fresh);
      setPool(pl);
      setAllVaults(all);
      setLoadError(undefined);
      if (accountId) {
        const [mine, usdc, shares, supplied, claimable, registered] = await Promise.all([
          app.vaultsOf(accountId),
          app.usdcBalance(c.usdc, accountId),
          app.sharesOf(accountId),
          app.suppliedOf(accountId),
          app.claimableOf(accountId),
          app.isRegisteredOn(c.usdc, accountId),
        ]);
        setMyVaults(mine);
        setBalances({
          usdc: BigInt(usdc),
          shares: BigInt(shares),
          supplied: BigInt(supplied),
          claimable: BigInt(claimable),
          registered,
        });
      } else {
        setMyVaults([]);
        setBalances(EMPTY_BALANCES);
      }
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e));
    }
  }, [accountId]);

  useEffect(() => {
    restoreSession().then((s) => s && setSession(s));
  }, []);

  useEffect(() => {
    refresh();
    const timer = setInterval(refresh, 15_000);
    return () => clearInterval(timer);
  }, [refresh]);

  const pushToast = useCallback((t: Omit<Toast, "id">): number => {
    const id = ++toastId.current;
    setToasts((list) => [...list, { ...t, id }]);
    return id;
  }, []);
  const updateToast = useCallback((id: number, patch: Partial<Toast>) => {
    setToasts((list) => list.map((t) => (t.id === id ? { ...t, ...patch } : t)));
    if (patch.kind === "ok") setTimeout(() => setToasts((list) => list.filter((t) => t.id !== id)), 12_000);
  }, []);

  const run = useCallback<Ctx["run"]>(
    async (title, action) => {
      const id = pushToast({ kind: "pending", title });
      setBusy(true);
      try {
        const done = await action();
        updateToast(id, { kind: "ok", detail: done?.detail, href: done?.href });
      } catch (e) {
        updateToast(id, { kind: "error", detail: e instanceof Error ? e.message : String(e) });
      } finally {
        setBusy(false);
        refresh();
      }
    },
    [pushToast, updateToast, refresh],
  );

  const send = useCallback<Ctx["send"]>(
    async (call) => {
      if (!session) throw new Error("connect a wallet first");
      const res = await session.send(call);
      if (!res.ok) throw new Error(res.error ?? "transaction failed");
      return res;
    },
    [session],
  );

  const ctx = useMemo<Ctx | undefined>(
    () =>
      cfg && pool
        ? { session, cfg, price, priceFresh, priceError, reports, pool, balances, busy, run, send, connect: () => setConnecting(true) }
        : undefined,
    [session, cfg, price, priceFresh, priceError, reports, pool, balances, busy, run, send],
  );

  return (
    <>
      <header className="top">
        <div className="top-inner">
          <a className="brand" href="./">
            <span className="mark" aria-hidden="true">
              <svg viewBox="0 0 32 32" width="22" height="22">
                <path d="M8 6v12a8 8 0 0 0 16 0V6" fill="none" stroke="currentColor" strokeWidth="2.4" strokeLinecap="square" />
              </svg>
            </span>
            Unwrapped
          </a>
          <nav className="sections" role="tablist" aria-label="Sections">
            {(
              [
                ["borrow", "Vaults"],
                ["lend", "Lending"],
                ["market", "Market"],
                ["how", "Method"],
              ] as [Tab, string][]
            ).map(([id, label]) => (
              <button key={id} role="tab" aria-selected={tab === id} className={tab === id ? "on" : ""} onClick={() => setTab(id)}>
                {label}
              </button>
            ))}
          </nav>
          <div className="top-right">
            {price ? (
              <span className="ticker" title={`Median of ${price.sources} independent reporters, spread ${(price.spread_bps / 100).toFixed(2)}%`}>
                <span className="ticker-label">BTC</span> ${formatUsdc(price.usdc_per_btc, 0)}
                <span className="ticker-sub">median · {price.sources}</span>
              </span>
            ) : (
              cfg && (
                <span className="ticker warn" title={priceError ?? undefined}>
                  <span className="ticker-label">BTC</span> no price
                </span>
              )
            )}
            {session ? (
              <button
                className="account"
                title="Sign out"
                onClick={async () => {
                  await session.signOut();
                  setSession(undefined);
                }}
              >
                {short(session.accountId, 14, 8)}
                {session.kind === "devkey" && <span className="tag">dev key</span>}
              </button>
            ) : (
              <button className="btn primary" onClick={() => setConnecting(true)}>
                Connect
              </button>
            )}
          </div>
        </div>
      </header>

      <main>
        <section className="lead">
          <div className="lead-text">
            <p className="kicker">
              Native Bitcoin credit · NEAR {D.network} · Bitcoin {D.btcNetwork}
            </p>
            <h1>
              Borrow against Bitcoin that <em>never leaves Bitcoin.</em>
            </h1>
            <p className="dek">
              No bridge, no wrapped token. A NEAR contract holds the only right to sign for your vault's native address.
            </p>
          </div>
          {pool && cfg && (
            <dl className="lead-figures">
              <div>
                <dt>Supplied</dt>
                <dd>${formatUsdc(pool.total_assets, 0)}</dd>
              </div>
              <div>
                <dt>Borrowed</dt>
                <dd>${formatUsdc(pool.total_debt, 0)}</dd>
              </div>
              <div>
                <dt>Borrow rate</dt>
                <dd>{pct(pool.borrow_apr_bps, 2)}</dd>
              </div>
              <div>
                <dt>Supply rate</dt>
                <dd>{pct(pool.supply_apr_bps, 2)}</dd>
              </div>
              <div>
                <dt>Max LTV</dt>
                <dd>{pct(cfg.max_ltv_bps, 0)}</dd>
              </div>
              <div>
                <dt>Vaults</dt>
                <dd>{allVaults.length}</dd>
              </div>
            </dl>
          )}
        </section>

        {loadError && <p className="notice error">Could not reach NEAR: {loadError}</p>}
        {!ctx && !loadError && <p className="notice">Loading chain state…</p>}

        {ctx && tab === "borrow" && <BorrowTab ctx={ctx} vaults={myVaults} />}
        {ctx && tab === "lend" && <LendPanel ctx={ctx} />}
        {ctx && tab === "market" && <MarketPanel ctx={ctx} vaults={allVaults} />}
        {ctx && tab === "how" && <HowItWorks ctx={ctx} />}
      </main>

      <footer>
        <div className="footer-inner">
          <span className="colophon">Unwrapped</span>
          <span>
            Contract{" "}
            <a href={nearAccountUrl(D.contract)} target="_blank" rel="noreferrer">
              {D.contract}
            </a>
          </span>
          <span>
            Signer{" "}
            <a href={nearAccountUrl(D.mpcSigner)} target="_blank" rel="noreferrer">
              {D.mpcSigner}
            </a>
          </span>
          <span>
            Light client{" "}
            <a href={nearAccountUrl(D.btcLightClient)} target="_blank" rel="noreferrer">
              {D.btcLightClient}
            </a>
          </span>
          <span>Testnet software. Test assets only.</span>
        </div>
      </footer>

      <div className="toasts" aria-live="polite">
        {toasts.map((t) => (
          <div key={t.id} className={`toast ${t.kind}`}>
            <div className="toast-head">
              <strong>{t.title}</strong>
              {t.kind !== "pending" && (
                <button aria-label="Dismiss" onClick={() => setToasts((list) => list.filter((x) => x.id !== t.id))}>
                  ×
                </button>
              )}
            </div>
            {t.kind === "pending" && <span className="muted">Waiting for the network…</span>}
            {t.detail && <span>{t.detail}</span>}
            {t.href && (
              <a href={t.href} target="_blank" rel="noreferrer">
                View transaction
              </a>
            )}
          </div>
        ))}
      </div>

      {connecting && (
        <ConnectDialog
          onClose={() => setConnecting(false)}
          onSession={(s) => {
            setSession(s);
            setConnecting(false);
          }}
        />
      )}
    </>
  );
}

function BorrowTab({ ctx, vaults }: { ctx: Ctx; vaults: Vault[] }) {
  const [selected, setSelected] = useState<number>();
  const current = vaults.find((v) => v.vault_id === selected) ?? vaults[0];

  const mint = () =>
    ctx.run("Creating a vault", async () => {
      const res = await ctx.send(app.mintVault(ctx.cfg.mint_fee));
      const v = res.value as Vault | null;
      if (!v) throw new Error("The signer did not return a key; the fee was refunded.");
      setSelected(v.vault_id);
      return { detail: `Vault No. ${v.vault_id} controls ${v.btc_address}`, href: res.txHash && nearTxUrl(res.txHash) };
    });

  if (!ctx.session) {
    return (
      <section className="empty">
        <p className="kicker">Vaults</p>
        <h2>A vault is an NFT that owns a Bitcoin address.</h2>
        <p>
          Connect a NEAR wallet to open one. Its key is generated by NEAR's MPC network, and only this contract can ask it
          to sign.
        </p>
        <button className="btn primary" onClick={ctx.connect}>
          Connect
        </button>
      </section>
    );
  }

  return (
    <section>
      <div className="section-head">
        <div className="vault-tabs" role="tablist" aria-label="Your vaults">
          <span className="kicker">Your vaults</span>
          {vaults.map((v) => (
            <button
              key={v.vault_id}
              role="tab"
              aria-selected={current?.vault_id === v.vault_id}
              className={current?.vault_id === v.vault_id ? "on" : ""}
              onClick={() => setSelected(v.vault_id)}
            >
              No. {v.vault_id}
              {v.locked && <span className="dot ochre" aria-label="secures a loan" />}
            </button>
          ))}
        </div>
        <button className="btn" disabled={ctx.busy} onClick={mint}>
          New vault
        </button>
      </div>
      {vaults.length === 0 && (
        <div className="empty">
          <p>
            You have no vaults yet. Creating one asks NEAR's MPC network for a fresh Bitcoin key that only this contract
            can sign with, and mints the NFT that controls it.
          </p>
        </div>
      )}
      {current && <VaultCard key={current.vault_id} ctx={ctx} vault={current} />}
    </section>
  );
}

function ConnectDialog({ onClose, onSession }: { onClose(): void; onSession(s: Session): void }) {
  const [accountId, setAccountId] = useState("");
  const [key, setKey] = useState("");
  const [error, setError] = useState<string>();
  const [showKey, setShowKey] = useState(false);

  const wrap = async (fn: () => Promise<Session> | Session) => {
    try {
      onSession(await fn());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="scrim" onClick={onClose}>
      <div className="dialog" role="dialog" aria-modal="true" onClick={(e) => e.stopPropagation()}>
        <h2>Connect</h2>
        <button className="btn primary wide" onClick={() => wrap(connectWallet)}>
          NEAR wallet
        </button>
        <button className="link" onClick={() => setShowKey((v) => !v)}>
          {showKey ? "Hide" : "Use a testnet key instead"}
        </button>
        {showKey && (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              wrap(() => connectDevKey(accountId, key));
            }}
          >
            <p className="muted small">
              For demos: paste a <strong>testnet</strong> account and its private key. It stays in this tab's session
              storage and is never sent anywhere but the NEAR RPC, as signatures.
            </p>
            <label>
              Account
              <input value={accountId} onChange={(e) => setAccountId(e.target.value)} placeholder="you.testnet" required />
            </label>
            <label>
              Private key
              <input value={key} onChange={(e) => setKey(e.target.value)} placeholder="ed25519:…" type="password" required />
            </label>
            <button className="btn" type="submit">
              Use this key
            </button>
          </form>
        )}
        {error && <p className="notice error">{error}</p>}
      </div>
    </div>
  );
}
