// Operations' own screens, at the top of each.

import { NavLink } from "react-router";

export function Tabs({ attention }: { attention?: number }) {
  return (
    <nav className="ops-tabs" aria-label="Operations">
      <NavLink to="/" end>
        Sites
      </NavLink>
      <NavLink to="/attention">Needs a person{attention ? ` (${attention})` : ""}</NavLink>
      <NavLink to="/commands">Commands</NavLink>
    </nav>
  );
}
