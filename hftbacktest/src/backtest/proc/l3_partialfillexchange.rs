use core::time;

use uuid::timestamp;

use crate::{
    backtest::{
        BacktestError,
        assettype::AssetType,
        models::{FeeModel, L3QueueModel, LatencyModel},
        order::{self, ExchToLocal},
        proc::Processor,
        state::State,
    },
    depth::{INVALID_MAX, INVALID_MIN, L3MarketDepth},
    prelude::OrdType,
    types::{
        AUCTION_UPDATE_EVENT, BUY_EVENT, DEPTH_CLEAR_EVENT, EXCH_ASK_ADD_ORDER_EVENT,
        EXCH_ASK_DEPTH_CLEAR_EVENT, EXCH_BID_ADD_ORDER_EVENT, EXCH_BID_DEPTH_CLEAR_EVENT,
        EXCH_CANCEL_ORDER_EVENT, EXCH_DEPTH_CLEAR_EVENT, EXCH_EVENT, EXCH_FILL_EVENT,
        EXCH_MODIFY_ORDER_EVENT, Event, Order, OrderId, SELL_EVENT, Side, Status, TimeInForce,
    },
};

pub struct L3PartialFillExchange<AT, LM, QM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    QM: L3QueueModel<MD>,
    MD: L3MarketDepth,
    FM: FeeModel,
{
    depth: MD,
    state: State<AT, FM>,
    queue_model: QM,
    order_e2l: ExchToLocal<LM>,

    auction_processed: bool,
}

impl<AT, LM, QM, MD, FM> L3PartialFillExchange<AT, LM, QM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    QM: L3QueueModel<MD>,
    MD: L3MarketDepth,
    FM: FeeModel,
    BacktestError: From<<MD as L3MarketDepth>::Error>,
{
    /// Constructs an instance of `L3PartialFillExchange`.
    pub fn new(
        depth: MD,
        state: State<AT, FM>,
        queue_model: QM,
        order_e2l: ExchToLocal<LM>,
    ) -> Self {
        println!("=== L3PartialFillExchange created ===");
        Self {
            depth,
            state,
            queue_model,
            order_e2l,

            auction_processed: false,
        }
    }

    fn expired(&mut self, mut order: Order, timestamp: i64) -> Result<(), BacktestError> {
        order.exec_qty = 0.0;
        order.leaves_qty = 0.0;
        order.status = Status::Expired;
        order.exch_timestamp = timestamp;

        self.order_e2l.respond(order);
        Ok(())
    }

    fn partial_fill<const MAKE_RESPONSE: bool>(
        &mut self,
        order: &mut Order,
        timestamp: i64,
        maker: bool,
        exec_price_tick: i64,
        fill_qty: f64,
    ) -> Result<(), BacktestError> {
        // println!("Partial fill: order_id={}, fill_qty={}, leaves_qty={}", order.order_id, fill_qty, order.leaves_qty);
        if order.status == Status::Expired
            || order.status == Status::Canceled
            || order.status == Status::Filled
        {
            return Err(BacktestError::InvalidOrderStatus);
        }

        // Ensure we don't fill more than available
        let actual_fill_qty = fill_qty.min(order.leaves_qty);

        order.maker = maker;
        if maker {
            order.exec_price_tick = order.price_tick;
        } else {
            order.exec_price_tick = exec_price_tick;
        }

        order.exec_qty = actual_fill_qty;
        order.leaves_qty -= actual_fill_qty;

        // Update status based on remaining quantity
        if order.leaves_qty <= 0.0 {
            order.status = Status::Filled;
        } else {
            order.status = Status::PartiallyFilled;
        }

        order.exch_timestamp = timestamp;

        self.state.apply_fill(order);

        if MAKE_RESPONSE {
            self.order_e2l.respond(order.clone());
        }
        Ok(())
    }

    fn fill_ask_orders_by_crossing(
        &mut self,
        prev_best_tick: i64,
        new_best_tick: i64,
        timestamp: i64,
    ) -> Result<(), BacktestError> {
        let filled = self
            .queue_model
            .on_best_bid_update(prev_best_tick, new_best_tick)?;
        for mut order in filled {
            let price_tick = order.price_tick;
            // For crossing orders, we assume full fill at the order's limit price
            let fill_qty = order.leaves_qty;
            self.partial_fill::<true>(&mut order, timestamp, true, price_tick, fill_qty)?;
        }
        Ok(())
    }

    fn fill_bid_orders_by_crossing(
        &mut self,
        prev_best_tick: i64,
        new_best_tick: i64,
        timestamp: i64,
    ) -> Result<(), BacktestError> {
        let filled = self
            .queue_model
            .on_best_ask_update(prev_best_tick, new_best_tick)?;
        for mut order in filled {
            let price_tick = order.price_tick;
            // For crossing orders, we assume full fill at the order's limit price
            let fill_qty = order.leaves_qty;
            self.partial_fill::<true>(&mut order, timestamp, true, price_tick, fill_qty)?;
        }
        Ok(())
    }

    fn try_fill_at_touch(
        &mut self,
        order: &mut Order,
        timestamp: i64,
    ) -> Result<bool, BacktestError> {
        if order.side == Side::Buy {
            let best_ask_tick = self.depth.best_ask_tick();
            if order.price_tick >= best_ask_tick {
                // Get available quantity at best ask
                let available_qty = self.depth.ask_qty_at_tick(best_ask_tick);
                if available_qty > 0.0 {
                    let fill_qty = available_qty.min(order.leaves_qty);
                    self.partial_fill::<false>(order, timestamp, false, best_ask_tick, fill_qty)?;
                    return Ok(true);
                }
            }
        } else {
            let best_bid_tick = self.depth.best_bid_tick();
            if order.price_tick <= best_bid_tick {
                // Get available quantity at best bid
                let available_qty = self.depth.bid_qty_at_tick(best_bid_tick);
                if available_qty > 0.0 {
                    let fill_qty = available_qty.min(order.leaves_qty);
                    self.partial_fill::<false>(order, timestamp, false, best_bid_tick, fill_qty)?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    // TODO unchecked
    fn ack_new(&mut self, order: &mut Order, timestamp: i64) -> Result<(), BacktestError> {
        if self.queue_model.contains_backtest_order(order.order_id) {
            return Err(BacktestError::OrderIdExist);
        }

        // Normal trading mode - with immediate matching
        match order.order_type {
            OrdType::Limit => {
                match order.time_in_force {
                    TimeInForce::GTC | TimeInForce::GTX => {
                        // Try immediate execution first
                        let filled = self.try_fill_at_touch(order, timestamp)?;

                        if order.leaves_qty > 0.0 {
                            // If not fully filled, add to book
                            if order.time_in_force == TimeInForce::GTX && filled {
                                // GTX order touched the market, expire remaining
                                order.status = Status::Expired;
                                order.exch_timestamp = timestamp;
                            } else {
                                // Add remaining quantity to book
                                order.status = if filled {
                                    Status::PartiallyFilled
                                } else {
                                    Status::New
                                };
                                order.exch_timestamp = timestamp;
                                self.queue_model
                                    .add_backtest_order(order.clone(), &self.depth)?;
                            }
                        }
                        Ok(())
                    }
                    TimeInForce::IOC => {
                        // Execute what we can and cancel the rest
                        self.try_fill_at_touch(order, timestamp)?;
                        if order.leaves_qty > 0.0 {
                            order.status = Status::Expired;
                            order.exch_timestamp = timestamp;
                        }
                        Ok(())
                    }
                    TimeInForce::FOK => {
                        // Check if full quantity can be filled
                        let can_fill_full = if order.side == Side::Buy {
                            let best_ask_tick = self.depth.best_ask_tick();
                            order.price_tick >= best_ask_tick
                                && self.depth.ask_qty_at_tick(best_ask_tick) >= order.leaves_qty
                        } else {
                            let best_bid_tick = self.depth.best_bid_tick();
                            order.price_tick <= best_bid_tick
                                && self.depth.bid_qty_at_tick(best_bid_tick) >= order.leaves_qty
                        };

                        if can_fill_full {
                            self.try_fill_at_touch(order, timestamp)?;
                        } else {
                            order.status = Status::Expired;
                            order.exch_timestamp = timestamp;
                        }
                        Ok(())
                    }
                    TimeInForce::Unsupported => Err(BacktestError::InvalidOrderRequest),
                }
            }
            OrdType::Market => {
                // Market orders try to fill against available liquidity
                if order.side == Side::Buy {
                    let mut remaining_qty = order.leaves_qty;
                    let mut tick = self.depth.best_ask_tick();

                    while remaining_qty > 0.0 && tick < self.depth.best_ask_tick() {
                        let available_qty = self.depth.ask_qty_at_tick(tick);
                        if available_qty > 0.0 {
                            let fill_qty = available_qty.min(remaining_qty);
                            self.partial_fill::<false>(order, timestamp, false, tick, fill_qty)?;
                            remaining_qty = order.leaves_qty;
                        }
                        tick += 1;
                    }
                } else {
                    let mut remaining_qty = order.leaves_qty;
                    let mut tick = self.depth.best_bid_tick();

                    while remaining_qty > 0.0 && tick > self.depth.best_bid_tick() {
                        let available_qty = self.depth.bid_qty_at_tick(tick);
                        if available_qty > 0.0 {
                            let fill_qty = available_qty.min(remaining_qty);
                            self.partial_fill::<false>(order, timestamp, false, tick, fill_qty)?;
                            remaining_qty = order.leaves_qty;
                        }
                        tick -= 1;
                    }
                }

                // If market order couldn't be fully filled, expire remaining
                if order.leaves_qty > 0.0 {
                    order.status = Status::Expired;
                    order.exch_timestamp = timestamp;
                }
                Ok(())
            }
            OrdType::Unsupported => Err(BacktestError::InvalidOrderRequest),
        }
    }

    // TODO unchecked
    fn ack_cancel(&mut self, order: &mut Order, timestamp: i64) -> Result<(), BacktestError> {
        match self
            .queue_model
            .cancel_backtest_order(order.order_id, &self.depth)
        {
            Ok(exch_order) => {
                let _ = std::mem::replace(order, exch_order);

                order.status = Status::Canceled;
                order.exch_timestamp = timestamp;
                Ok(())
            }
            Err(BacktestError::OrderNotFound) => {
                order.req = Status::Rejected;
                order.exch_timestamp = timestamp;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    // TODO unchecked
    fn ack_modify<const RESET_QUEUE_POS: bool>(
        &mut self,
        order: &mut Order,
        timestamp: i64,
    ) -> Result<(), BacktestError> {
        match self
            .queue_model
            .modify_backtest_order(order.order_id, order, &self.depth)
        {
            Ok(()) => {
                order.exch_timestamp = timestamp;
                Ok(())
            }
            Err(BacktestError::OrderNotFound) => {
                order.req = Status::Rejected;
                order.exch_timestamp = timestamp;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl<AT, LM, QM, MD, FM> Processor for L3PartialFillExchange<AT, LM, QM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    QM: L3QueueModel<MD>,
    MD: L3MarketDepth,
    FM: FeeModel,
    BacktestError: From<<MD as L3MarketDepth>::Error>,
{
    fn event_seen_timestamp(&self, event: &Event) -> Option<i64> {
        event.is(EXCH_EVENT).then_some(event.exch_ts)
    }

    fn process(&mut self, event: &Event) -> Result<(), BacktestError> {
        if !event.is(AUCTION_UPDATE_EVENT) {
            self.depth.set_allow_price_cross(false);
            self.auction_processed = false;
        } else if event.is(AUCTION_UPDATE_EVENT) {
            self.depth.set_allow_price_cross(true);
        }

        if event.is(EXCH_BID_DEPTH_CLEAR_EVENT) {
            self.depth.clear_orders(Side::Buy);
            let expired = self.queue_model.clear_orders(Side::Buy);
            for order in expired {
                self.expired(order, event.exch_ts)?;
            }
        } else if event.is(EXCH_ASK_DEPTH_CLEAR_EVENT) {
            self.depth.clear_orders(Side::Sell);
            let expired = self.queue_model.clear_orders(Side::Sell);
            for order in expired {
                self.expired(order, event.exch_ts)?;
            }
        } else if event.is(EXCH_DEPTH_CLEAR_EVENT) {
            // println!("[EXCHANGE] FULL DEPTH CLEAR");
            self.depth.clear_orders(Side::None);
            let expired = self.queue_model.clear_orders(Side::None);
            for order in expired {
                self.expired(order, event.exch_ts)?;
            }
        } else if event.is(EXCH_BID_ADD_ORDER_EVENT) {
            // println!("exch");
            let (prev_best_bid_tick, best_bid_tick) =
                self.depth
                    .add_buy_order(event.order_id, event.px, event.qty, event.exch_ts)?;
            self.queue_model.add_market_feed_order(event, &self.depth)?;

            // println!("[EXCHANGE] BID added: prev_best={}, new_best={}", prev_best_bid_tick, best_bid_tick);

            if !event.is(AUCTION_UPDATE_EVENT) && best_bid_tick > prev_best_bid_tick {
                // println!("ask partial fill crossing fill!");
                self.fill_ask_orders_by_crossing(prev_best_bid_tick, best_bid_tick, event.exch_ts)?;
            }
        } else if event.is(EXCH_ASK_ADD_ORDER_EVENT) {
            // println!("exch");
            let (prev_best_ask_tick, best_ask_tick) =
                self.depth
                    .add_sell_order(event.order_id, event.px, event.qty, event.exch_ts)?;
            self.queue_model.add_market_feed_order(event, &self.depth)?;

            // println!("[EXCHANGE] ASK added: prev_best={}, new_best={}", prev_best_ask_tick, best_ask_tick);

            if !event.is(AUCTION_UPDATE_EVENT) && best_ask_tick < prev_best_ask_tick {
                // println!("bid partial fill crossing fill!");
                self.fill_bid_orders_by_crossing(prev_best_ask_tick, best_ask_tick, event.exch_ts)?;
            }
        } else if event.is(EXCH_CANCEL_ORDER_EVENT) {
            let order_id = event.order_id;
            self.depth.delete_order(order_id, event.exch_ts)?;
            self.queue_model
                .cancel_market_feed_order(event.order_id, &self.depth)?;
        } else if event.is(EXCH_FILL_EVENT) {
            if event.is(BUY_EVENT) || event.is(SELL_EVENT) {
                // println!("[EXCHANGE] Processing FILL event for market feed order");
                let filled = self.queue_model.fill_market_feed_order::<false>(
                    event.order_id,
                    event,
                    &self.depth,
                )?;
                let timestamp = event.exch_ts;
                let fill_qty = event.qty; // The quantity from the market feed fill event
                for mut order in filled {
                    // Partial fill based on the market feed fill quantity
                    // This assumes FIFO - front orders get filled first
                    let order_fill_qty = fill_qty.min(order.leaves_qty);
                    let price_tick = order.price_tick;
                    self.partial_fill::<true>(
                        &mut order,
                        timestamp,
                        true,
                        price_tick,
                        order_fill_qty,
                    )?;
                }
            } else if event.is(AUCTION_UPDATE_EVENT) && !self.auction_processed {
                self.auction_processed = true;

                let auction_price = event.px;
                let auction_price_tick = (auction_price / self.depth.tick_size()).round() as i64;
                let timestamp = event.exch_ts;

                println!(
                    "[AUCTION] Processing auction at price: {} (tick: {})",
                    auction_price, auction_price_tick
                );

                // 1. 获取所有能成交的订单
                // 买单：价格 >= 集合竞价价格
                let mut filled_bids = Vec::new();
                let mut bids_at_auction_price = Vec::new();
                let mut total_bid_qty_ge_auction = 0.0;

                let all_bid_orders = self.queue_model.get_all_bid_orders();
                for order in all_bid_orders {
                    if order.price_tick > auction_price_tick {
                        total_bid_qty_ge_auction += order.leaves_qty;
                        filled_bids.push(order);
                    } else if order.price_tick == auction_price_tick {
                        total_bid_qty_ge_auction += order.leaves_qty;
                        bids_at_auction_price.push(order);
                    }
                }

                // 卖单：价格 <= 集合竞价价格
                let mut filled_asks = Vec::new();
                let mut asks_at_auction_price = Vec::new();
                let mut total_ask_qty_le_auction = 0.0;

                let all_ask_orders = self.queue_model.get_all_ask_orders();
                for order in all_ask_orders {
                    if order.price_tick < auction_price_tick {
                        total_ask_qty_le_auction += order.leaves_qty;
                        filled_asks.push(order);
                    } else if order.price_tick == auction_price_tick {
                        total_ask_qty_le_auction += order.leaves_qty;
                        asks_at_auction_price.push(order);
                    }
                }

                println!(
                    "[AUCTION] Orders above/below auction price - Bids: {}, Asks: {}",
                    filled_bids.len(),
                    filled_asks.len()
                );
                println!(
                    "[AUCTION] Orders at auction price - Bids: {} (qty: {}), Asks: {} (qty: {})",
                    bids_at_auction_price.len(),
                    total_bid_qty_ge_auction,
                    asks_at_auction_price.len(),
                    total_ask_qty_le_auction
                );

                // 2. 处理价格优于集合竞价价格的订单（全部成交）
                for mut order in filled_bids {
                    let order_id = order.order_id;
                    let order_leaves_qty = order.leaves_qty;

                    self.depth.delete_order(order_id, timestamp)?;
                    self.queue_model
                        .cancel_market_feed_order(order_id, &self.depth)?;
                }

                for mut order in filled_asks {
                    let order_id = order.order_id;
                    let order_leaves_qty = order.leaves_qty;

                    self.depth.delete_order(order_id, timestamp)?;
                    self.queue_model
                        .cancel_market_feed_order(order_id, &self.depth)?;
                }

                // 3. 处理价格等于集合竞价价格的订单
                if !bids_at_auction_price.is_empty() || !asks_at_auction_price.is_empty() {
                    if total_bid_qty_ge_auction <= total_ask_qty_le_auction {
                        // 买单数量少，买单全部成交
                        for order in bids_at_auction_price {
                            let order_id = order.order_id;

                            self.depth.delete_order(order_id, timestamp)?;
                            self.queue_model
                                .cancel_market_feed_order(order_id, &self.depth)?;
                        }

                        // left_qty 是开盘后剩余在卖1的数量
                        let left_qty = total_ask_qty_le_auction - total_bid_qty_ge_auction;
                        // 计算卖单的总量
                        let mut total_asks_qty = 0.0;
                        for order in &asks_at_auction_price {
                            total_asks_qty += order.leaves_qty;
                        }
                        // 需要成交的总量
                        let need_to_fill = total_asks_qty - left_qty;
                        let mut already_filled = 0.0;

                        for mut order in asks_at_auction_price {
                            if already_filled >= need_to_fill {
                                break; // 剩余订单保留在订单簿中
                            }

                            // 这个订单需要成交的量
                            let order_fill_qty =
                                (need_to_fill - already_filled).min(order.leaves_qty);
                            already_filled += order_fill_qty;

                            if order_fill_qty >= order.leaves_qty {
                                // 订单完全成交
                                self.depth.delete_order(order.order_id, timestamp)?;
                                self.queue_model
                                    .cancel_market_feed_order(order.order_id, &self.depth)?;
                            } else if order_fill_qty > 0.0 {
                                // 订单部分成交
                                let remaining_qty = order.leaves_qty - order_fill_qty;

                                // 更新深度中该订单的数量
                                self.depth.modify_order(
                                    order.order_id,
                                    auction_price,
                                    remaining_qty,
                                    timestamp,
                                )?;
                            }

                            // pass an order is_auction
                            order.exec_price_tick = auction_price_tick;
                            order.qty = left_qty;
                            order.is_auction = true;
                            self.order_e2l.respond(order.clone());
                        }
                    } else {
                        // 卖单数量少，卖单全部成交
                        for order in asks_at_auction_price {
                            let order_id = order.order_id;

                            self.depth.delete_order(order_id, timestamp)?;
                            self.queue_model
                                .cancel_market_feed_order(order_id, &self.depth)?;
                        }

                        let left_qty = total_bid_qty_ge_auction - total_ask_qty_le_auction;

                        let mut total_bids_qty = 0.0;
                        for order in &bids_at_auction_price {
                            total_bids_qty += order.leaves_qty;
                        }

                        let need_to_fill = total_bids_qty - left_qty;
                        let mut already_filled = 0.0;

                        for mut order in bids_at_auction_price {
                            if already_filled >= need_to_fill {
                                break;
                            }

                            let order_fill_qty =
                                (need_to_fill - already_filled).min(order.leaves_qty);
                            already_filled += order_fill_qty;

                            if order_fill_qty >= order.leaves_qty {
                                // 订单完全成交
                                self.depth.delete_order(order.order_id, timestamp)?;
                                self.queue_model
                                    .cancel_market_feed_order(order.order_id, &self.depth)?;
                            } else if order_fill_qty > 0.0 {
                                // 订单部分成交
                                let remaining_qty = order.leaves_qty - order_fill_qty;

                                // 更新深度中该订单的数量
                                self.depth.modify_order(
                                    order.order_id,
                                    auction_price,
                                    remaining_qty,
                                    timestamp,
                                )?;
                            }
                            // pass an order is_auction
                            order.exec_price_tick = auction_price_tick;
                            order.qty = -left_qty;
                            order.is_auction = true;
                            self.order_e2l.respond(order.clone());
                        }
                    }

                    println!(
                        "[AUCTION] Auction completed. Opening price: {}",
                        auction_price
                    );

                    // 打印5档深度
                    println!("[AUCTION] Post-auction market depth (5 levels):");
                    println!("         Bid                    Ask");
                    println!("  Price      Qty        Price      Qty");
                    println!("---------- --------   ---------- --------");

                    // 获取5档深度
                    let mut bid_levels = Vec::new();
                    let mut ask_levels = Vec::new();

                    // 获取买档 - 从最优买价开始向下查找
                    if self.depth.best_bid_tick() != INVALID_MIN {
                        let tick_size = self.depth.tick_size();
                        let mut current_tick = self.depth.best_bid_tick();

                        for _ in 0..5 {
                            let qty = self.depth.bid_qty_at_tick(current_tick);
                            if qty > 0.0 {
                                let price = current_tick as f64 * tick_size;
                                bid_levels.push((price, qty));
                            }

                            // 向下查找下一个有效价格档位
                            let mut found_next = false;
                            for i in 1..=100 {
                                // 最多查找100个tick
                                let next_tick = current_tick - i;
                                if self.depth.bid_qty_at_tick(next_tick) > 0.0 {
                                    current_tick = next_tick;
                                    found_next = true;
                                    break;
                                }
                            }

                            if !found_next {
                                break;
                            }
                        }
                    }

                    // 获取卖档 - 从最优卖价开始向上查找
                    if self.depth.best_ask_tick() != INVALID_MAX {
                        let tick_size = self.depth.tick_size();
                        let mut current_tick = self.depth.best_ask_tick();

                        for _ in 0..5 {
                            let qty = self.depth.ask_qty_at_tick(current_tick);
                            if qty > 0.0 {
                                let price = current_tick as f64 * tick_size;
                                ask_levels.push((price, qty));
                            }

                            // 向上查找下一个有效价格档位
                            let mut found_next = false;
                            for i in 1..=100 {
                                // 最多查找100个tick
                                let next_tick = current_tick + i;
                                if self.depth.ask_qty_at_tick(next_tick) > 0.0 {
                                    current_tick = next_tick;
                                    found_next = true;
                                    break;
                                }
                            }

                            if !found_next {
                                break;
                            }
                        }
                    }

                    // 打印深度表格
                    for i in 0..5 {
                        let bid_str = if i < bid_levels.len() {
                            format!("{:10.2} {:8.0}", bid_levels[i].0, bid_levels[i].1)
                        } else {
                            format!("{:10} {:8}", "--", "--")
                        };

                        let ask_str = if i < ask_levels.len() {
                            format!("{:10.2} {:8.0}", ask_levels[i].0, ask_levels[i].1)
                        } else {
                            format!("{:10} {:8}", "--", "--")
                        };

                        println!("{}   {}", bid_str, ask_str);
                    }

                    // 打印最优买卖价和价差
                    if self.depth.best_bid_tick() != INVALID_MIN
                        && self.depth.best_ask_tick() != INVALID_MAX
                    {
                        let best_bid = self.depth.best_bid();
                        let best_ask = self.depth.best_ask();
                        let spread = best_ask - best_bid;
                        let spread_ticks = self.depth.best_ask_tick() - self.depth.best_bid_tick();
                        let mid_price = (best_bid + best_ask) / 2.0;

                        println!();
                        println!("[AUCTION] Summary:");
                        println!(
                            "  Best Bid: {:.2} (qty: {:.0})",
                            best_bid,
                            self.depth.bid_qty_at_tick(self.depth.best_bid_tick())
                        );
                        println!(
                            "  Best Ask: {:.2} (qty: {:.0})",
                            best_ask,
                            self.depth.ask_qty_at_tick(self.depth.best_ask_tick())
                        );
                        println!("  Mid Price: {:.2}", mid_price);
                        println!("  Spread: {:.2} ({} ticks)", spread, spread_ticks);
                    } else if self.depth.best_bid_tick() != INVALID_MIN {
                        println!();
                        println!("[AUCTION] Only bid side has orders");
                        println!(
                            "  Best Bid: {:.2} (qty: {:.0})",
                            self.depth.best_bid(),
                            self.depth.bid_qty_at_tick(self.depth.best_bid_tick())
                        );
                    } else if self.depth.best_ask_tick() != INVALID_MAX {
                        println!();
                        println!("[AUCTION] Only ask side has orders");
                        println!(
                            "  Best Ask: {:.2} (qty: {:.0})",
                            self.depth.best_ask(),
                            self.depth.ask_qty_at_tick(self.depth.best_ask_tick())
                        );
                    } else {
                        println!();
                        println!("[AUCTION] No orders in the book");
                    }
                }
            }
        }
        Ok(())
    }

    // TODO unchecked
    fn process_recv_order(
        &mut self,
        timestamp: i64,
        wait_resp_order_id: Option<OrderId>,
    ) -> Result<bool, BacktestError> {
        while let Some(mut order) = self.order_e2l.receive(timestamp) {
            // Processes a new order.
            if order.req == Status::New {
                order.req = Status::None;
                self.ack_new(&mut order, timestamp)?;
            }
            // Processes a cancel order.
            else if order.req == Status::Canceled {
                order.req = Status::None;
                self.ack_cancel(&mut order, timestamp)?;
            }
            // Processes a modify order.
            else if order.req == Status::Replaced {
                order.req = Status::None;
                self.ack_modify::<false>(&mut order, timestamp)?;
            } else {
                return Err(BacktestError::InvalidOrderRequest);
            }
            // Makes the response.
            self.order_e2l.respond(order);
        }
        Ok(false)
    }

    fn earliest_recv_order_timestamp(&self) -> i64 {
        self.order_e2l
            .earliest_recv_order_timestamp()
            .unwrap_or(i64::MAX)
    }

    fn earliest_send_order_timestamp(&self) -> i64 {
        self.order_e2l
            .earliest_send_order_timestamp()
            .unwrap_or(i64::MAX)
    }
}
