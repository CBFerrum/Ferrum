// Operations' own screens, at the top of each.

import { NavLink } from "react-router";
import { t } from "@ferrum/client";

export function Tabs({ attention }: { attention?: number }) {
  return (
    <nav className="ops-tabs" aria-label={t("Operations")}>
      <NavLink to="/" end>
        {t("Sites")}
      </NavLink>
      <NavLink to="/attention">{t("Needs a person")}{attention ? ` (${attention})` : ""}</NavLink>
      <NavLink to="/commands">{t("Commands")}</NavLink>
    </nav>
  );
}
