// Edge's own screens, at the top of each.

import { NavLink } from "react-router";

export function Tabs({ alarms }: { alarms?: number }) {
  return (
    <nav className="edge-tabs" aria-label="IoT Edge">
      <NavLink to="/" end>
        Gateways
      </NavLink>
      <NavLink to="/devices">Devices</NavLink>
      <NavLink to="/alarms">Alarms{alarms ? ` (${alarms})` : ""}</NavLink>
      <NavLink to="/robots">Robots</NavLink>
      <NavLink to="/flows">Flows</NavLink>
      <NavLink to="/definitions">Definitions</NavLink>
    </nav>
  );
}
