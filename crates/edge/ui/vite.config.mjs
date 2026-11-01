// IoT Edge's own interface (docs/plan/edge.md), built with Ferrum-Frontend's packages and libraries
// from the checkout at ../../../web.
import { ferrumApp } from "../../../web/packages/build/vite.mjs";

export default ferrumApp(import.meta.url);
