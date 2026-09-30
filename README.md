# Stock App

A marketplace where sellers open numbered shops, list products, take orders and confirm bank-transfer payments.

## Features

- Seller accounts with numbered shop links (0001, 0002, …)
- Products with photos, sizes, colours, price and stock
- Stock management: low-stock warnings, quick restock (+1 / +5 / +10), per-product stock history
- Shopping cart: several products in one order, paid in one transfer
- Orders & payments: confirm payment, mark as sent, mark as delivered
- Sales dashboard: revenue per day (last 30 days), best sellers, orders by status

## How it runs

Everything is in one file, `index.html`, with plain HTML, CSS and JavaScript and no build step.

The data layer uses the claude.ai artifact runtime (`window.claude.use("db")` and `window.claude.use("user")`).
If you open `index.html` directly in a browser or from GitHub Pages, it shows the
"Open it from its claude.ai link" screen, because that storage isn't available outside claude.ai.

To run it on its own, replace the `db` and `user` calls in the `boot()` function with a backend
such as Firebase or Supabase. The app only uses `collection().onSnapshot()`, `collection().add()`,
`doc().get()`, `doc().set()`, `doc().update()`, `doc().delete()` and `doc().acquire()`, so a small adapter is enough.

## Data collections

| Collection | What it holds |
|---|---|
| `shops` | One document per shop, keyed by shop number |
| `products` | Products, each with `shopNo`, `price`, `quantity`, `lowStock` |
| `orders` | Orders, with an `items` array (older orders have a single `productId`) |
| `stockLog` | Stock changes: `change`, `reason` (restock, sale, returned, edit), `after`, `at` |
| `data/users/{uid}/profile` | Seller profile and bank details |
