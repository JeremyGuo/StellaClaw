export function TerminalDock({ open }) {
  if (!open) return null;
  return (
    <section className="terminal-dock">
      <header>终端</header>
      <pre>$ echo Stellacode 2 terminal dock</pre>
    </section>
  );
}
