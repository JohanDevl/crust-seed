import { cn } from "@/lib/utils";

/**
 * The crust-seed mark: a hexagonal shell — the crust — around a single seed.
 *
 * Kept as a component rather than an imported `.svg` so the same geometry can
 * be reused at any size (sidebar, login card, loading state, favicon build)
 * without shipping three near-identical files that can drift apart.
 *
 * The two brand colours are literal on purpose. A logo that re-tints itself
 * from the theme stops being an identity, and this one has to stay legible on
 * the sidebar, on a white login card and on the dark canvas alike.
 */

const SHELL =
  "M36.68,59.30Q32.00,62.00 27.32,59.30L10.70,49.70Q6.02,47.00 6.02,41.60L6.02,22.40Q6.02,17.00 10.70,14.30L27.32,4.70Q32.00,2.00 36.68,4.70L53.30,14.30Q57.98,17.00 57.98,22.40L57.98,41.60Q57.98,47.00 53.30,49.70Z";
const CORE =
  "M35.29,51.10Q32.00,53.00 28.71,51.10L17.10,44.40Q13.81,42.50 13.81,38.70L13.81,25.30Q13.81,21.50 17.10,19.60L28.71,12.90Q32.00,11.00 35.29,12.90L46.90,19.60Q50.19,21.50 50.19,25.30L50.19,38.70Q50.19,42.50 46.90,44.40Z";
const SEED =
  "M32.00,18.40C41.22,27.38 41.22,36.62 32.00,45.60C22.78,36.62 22.78,27.38 32.00,18.40Z";

export function LogoMark({
  className,
  title = "crust-seed",
}: {
  className?: string;
  title?: string;
}) {
  return (
    <svg
      viewBox="0 0 64 64"
      xmlns="http://www.w3.org/2000/svg"
      role="img"
      aria-label={title}
      className={cn("shrink-0", className)}
    >
      <path fill="#d9541e" fillRule="evenodd" d={`${SHELL} ${CORE}`} />
      <path fill="#f2a93b" d={SEED} />
    </svg>
  );
}
