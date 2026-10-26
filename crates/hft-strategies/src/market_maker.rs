use hft_book::Book;
use hft_data::{MboEvent, Price, Qty, Side};
use hft_sim::{ClientOrderId, Ctx, OrderUpdate, Strategy, TimeInForce};

#[derive(Clone, Copy, Debug)]
pub struct MarketMakerConfig {
    pub qty: Qty,
    pub max_position: i64,
    /// Only quote when the spread is at least this wide (price units).
    pub min_spread: Price,
}

#[derive(Clone, Copy, Debug, Default)]
struct Quote {
    id: Option<ClientOrderId>,
    price: Price,
    canceling: bool,
}

/// Joins the best bid and ask with a fixed size, steps aside on a side when the
/// position limit is hit, and requotes whenever the touch moves.
///
/// A baseline to exercise the simulator, not a strategy expected to make money.
pub struct MarketMaker {
    cfg: MarketMakerConfig,
    quotes: [Quote; 2],
}

impl MarketMaker {
    pub fn new(cfg: MarketMakerConfig) -> Self {
        MarketMaker {
            cfg,
            quotes: [Quote::default(); 2],
        }
    }

    fn requote(&mut self, ctx: &mut Ctx, book: &Book) {
        let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) else {
            return;
        };
        let wide = ask - bid >= self.cfg.min_spread;
        let pos = ctx.position();
        let room = |sign: i64| (pos + sign * self.cfg.qty as i64).abs() <= self.cfg.max_position;
        let targets = [
            (Side::Bid, (wide && room(1)).then_some(bid)),
            (Side::Ask, (wide && room(-1)).then_some(ask)),
        ];

        for (q, (side, target)) in self.quotes.iter_mut().zip(targets) {
            match (q.id, target) {
                (Some(id), t) if t != Some(q.price) && !q.canceling => {
                    ctx.cancel(id);
                    q.canceling = true;
                }
                (None, Some(price)) => {
                    *q = Quote {
                        id: Some(ctx.send(side, price, self.cfg.qty, TimeInForce::Gtc)),
                        price,
                        canceling: false,
                    };
                }
                _ => {}
            }
        }
    }
}

impl Strategy for MarketMaker {
    fn on_market_data(&mut self, ctx: &mut Ctx, book: &Book, _: &MboEvent) {
        self.requote(ctx, book);
    }

    fn on_order_update(&mut self, ctx: &mut Ctx, book: &Book, update: &OrderUpdate) {
        if let OrderUpdate::Filled { id, leaves: 0, .. } | OrderUpdate::Canceled { id, .. } =
            *update
        {
            for q in &mut self.quotes {
                if q.id == Some(id) {
                    *q = Quote::default();
                }
            }
        }
        self.requote(ctx, book);
    }
}
