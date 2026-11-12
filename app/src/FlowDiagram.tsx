import { formatBtc, formatUsdc, type Vault } from "@unwrapped/sdk";
import { D, short } from "./chain.ts";

/**
 * Where a vault's value actually sits, drawn as a schematic with two bands:
 * Bitcoin on top, NEAR below. Every label comes from live chain state, and an
 * edge is drawn solid only while it carries something (proven BTC, an open
 * loan); otherwise it is a quiet dashed rule.
 *
 * Two drawings of the same schematic: a wide one, and a tall one for phones
 * so that nothing has to be scrolled sideways or shrunk below legibility.
 */
export function FlowDiagram({ vault, onBitcoinSats }: { vault: Vault; onBitcoinSats: number | null }) {
  const proven = BigInt(vault.collateral_sats);
  const debt = BigInt(vault.debt);
  const s: Schematic = {
    vault,
    proven,
    debt,
    hasCollateral: proven > 0n,
    hasLoan: debt > 0n,
    addressLive: proven > 0n || (onBitcoinSats ?? 0) > 0,
    onBitcoin: onBitcoinSats === null ? "reading Bitcoin…" : `${formatBtc(onBitcoinSats)} BTC on chain`,
    outputs: vault.utxos.length,
  };
  const title = `Vault ${vault.vault_id}: ${formatBtc(proven)} BTC proven on Bitcoin, ${formatUsdc(debt)} USDC borrowed on NEAR`;

  return (
    <figure className="flow" aria-label={`Where vault ${vault.vault_id}'s value sits`}>
      <Wide s={s} title={title} />
      <Tall s={s} title={title} />
      <figcaption>
        The BTC never leaves Bitcoin. NEAR holds only the right to sign for it, and the contract decides when that right
        may be used.
      </figcaption>
    </figure>
  );
}

interface Schematic {
  vault: Vault;
  proven: bigint;
  debt: bigint;
  hasCollateral: boolean;
  hasLoan: boolean;
  addressLive: boolean;
  onBitcoin: string;
  outputs: number;
}

const edge = (active: boolean) => (active ? "edge on" : "edge");
const head = (active: boolean) => `url(#arrow-${active ? "on" : "off"})`;

function Markers() {
  return (
    <defs>
      <marker id="arrow-on" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
        <path d="M0 0 L10 5 L0 10 z" className="arrowhead on" />
      </marker>
      <marker id="arrow-off" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
        <path d="M0 0 L10 5 L0 10 z" className="arrowhead" />
      </marker>
    </defs>
  );
}

function Node(props: {
  x: number;
  y: number;
  w: number;
  h?: number;
  kind?: string;
  kicker: string;
  main: string;
  mainClass: "node-mono" | "node-value";
  sub: string;
}) {
  const h = props.h ?? 70;
  return (
    <g transform={`translate(${props.x} ${props.y})`}>
      <rect width={props.w} height={h} className={`node ${props.kind ?? ""}`} />
      <text x="14" y="20" className="node-kicker">
        {props.kicker}
      </text>
      <text x="14" y="40" className={props.mainClass}>
        {props.main}
      </text>
      <text x="14" y="58" className="node-sub">
        {props.sub}
      </text>
    </g>
  );
}

function nodes(s: Schematic) {
  const { vault } = s;
  return {
    address: {
      kicker: "NATIVE ADDRESS",
      main: short(vault.btc_address, 14, 10),
      mainClass: "node-mono" as const,
      sub: s.onBitcoin,
      kind: s.addressLive ? "on" : "",
    },
    mpc: {
      kicker: "CHAIN SIGNATURES · MPC",
      main: D.mpcSigner,
      mainClass: "node-mono" as const,
      sub: `holds the key · path ${vault.derivation_path}`,
    },
    client: {
      kicker: "LIGHT CLIENT",
      main: short(D.btcLightClient, 16, 4),
      mainClass: "node-mono" as const,
      sub: `${s.outputs} deposit${s.outputs === 1 ? "" : "s"} proven`,
      kind: s.hasCollateral ? "on" : "",
    },
    vault: {
      kicker: `VAULT NFT · NO. ${vault.vault_id}`,
      main: `${formatBtc(s.proven)} BTC`,
      mainClass: "node-value" as const,
      sub: s.hasLoan ? "locked · secures a loan" : "free to move",
      kind: `strong ${s.hasLoan ? "locked" : ""}`,
    },
    pool: {
      kicker: "LENDING POOL",
      main: `${formatUsdc(s.debt)} USDC`,
      mainClass: "node-value" as const,
      sub: s.hasLoan ? `borrowed by ${short(vault.owner_id, 10, 8)}` : "no loan open",
      kind: s.hasLoan ? "on" : "",
    },
  };
}

function Wide({ s, title }: { s: Schematic; title: string }) {
  const n = nodes(s);
  return (
    <svg viewBox="0 0 760 316" role="img" className="flow-wide">
      <title>{title}</title>
      <Markers />
      <rect x="0" y="0" width="760" height="128" className="band btc" />
      <rect x="0" y="128" width="760" height="188" className="band near" />
      <line x1="0" y1="128" x2="760" y2="128" className="band-rule" />
      <text x="16" y="24" className="band-label">BITCOIN</text>
      <text x="16" y="152" className="band-label">NEAR</text>

      <Node x={64} y={38} w={270} {...n.address} />
      <Node x={450} y={38} w={270} {...n.mpc} />
      <path d="M450 73 L338 73" className={edge(false)} markerEnd={head(false)} />
      <text x="394" y="65" textAnchor="middle" className="edge-label">signs withdrawals</text>
      <text x="394" y="89" textAnchor="middle" className="edge-label faint">the contract built</text>

      <Node x={64} y={196} w={176} {...n.client} />
      <Node x={292} y={196} w={176} {...n.vault} />
      <Node x={520} y={196} w={200} {...n.pool} />

      <path d="M152 108 L152 192" className={edge(s.hasCollateral)} markerEnd={head(s.hasCollateral)} />
      <text x="162" y="150" className="edge-label">SPV proof</text>
      <text x="162" y="166" className="edge-label faint">{`${D.btcNetwork} · merkle + coinbase`}</text>

      <path d="M240 231 L288 231" className={edge(s.hasCollateral)} markerEnd={head(s.hasCollateral)} />

      <path d="M440 196 L560 112" className={edge(true)} markerEnd={head(true)} />
      <text x="512" y="166" className="edge-label">sole signer</text>
      <text x="512" y="182" className="edge-label faint">{short(D.contract, 12, 8)}</text>

      <path d="M516 231 L472 231" className={edge(s.hasLoan)} markerEnd={head(s.hasLoan)} />
      <text x="494" y="292" textAnchor="middle" className="edge-label faint">
        {s.hasLoan ? `loan ${formatUsdc(s.debt)}` : "no loan"}
      </text>
    </svg>
  );
}

function Tall({ s, title }: { s: Schematic; title: string }) {
  const n = nodes(s);
  return (
    <svg viewBox="0 0 360 572" role="img" className="flow-tall">
      <title>{title}</title>
      <Markers />
      <rect x="0" y="0" width="360" height="246" className="band btc" />
      <rect x="0" y="246" width="360" height="326" className="band near" />
      <line x1="0" y1="246" x2="360" y2="246" className="band-rule" />
      <text x="12" y="20" className="band-label">BITCOIN</text>
      <text x="12" y="266" className="band-label">NEAR</text>

      <Node x={20} y={32} w={320} {...n.address} />
      <Node x={130} y={156} w={210} {...n.mpc} sub={`path ${s.vault.derivation_path}`} />
      <path d="M235 156 L235 106" className={edge(false)} markerEnd={head(false)} />
      <text x="243" y="126" className="edge-label">signs withdrawals</text>
      <text x="243" y="141" className="edge-label faint">the contract built</text>

      <path d="M50 102 L50 282" className={edge(s.hasCollateral)} markerEnd={head(s.hasCollateral)} />
      <text x="60" y="196" className="edge-label">SPV proof</text>
      <text x="60" y="211" className="edge-label faint">merkle proof</text>

      <Node x={20} y={286} w={260} h={64} {...n.client} />
      <path d="M50 350 L50 382" className={edge(s.hasCollateral)} markerEnd={head(s.hasCollateral)} />

      <Node x={20} y={386} w={320} h={64} {...n.vault} />
      <path d="M310 386 L310 230" className={edge(true)} markerEnd={head(true)} />
      <text x="302" y="274" textAnchor="end" className="edge-label">sole signer</text>

      <Node x={20} y={486} w={320} h={64} {...n.pool} />
      <path d="M180 486 L180 454" className={edge(s.hasLoan)} markerEnd={head(s.hasLoan)} />
      <text x="190" y="474" className="edge-label faint">
        {s.hasLoan ? `loan ${formatUsdc(s.debt)}` : "no loan"}
      </text>
    </svg>
  );
}
