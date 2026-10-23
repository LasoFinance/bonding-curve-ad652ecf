use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer, Transfer};
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{
    mint_to, set_authority, transfer_checked, Mint, MintTo, SetAuthority, TokenAccount,
    TokenInterface, TransferChecked,
};
use anchor_spl::token_interface::spl_token_2022::instruction::AuthorityType;

declare_id!("Bf52dgDvUe5AjxoGtKHPwh27dwokY3FxEdoEtfuWAwFC");

pub const DECIMALS: u8 = 6;
pub const TOTAL_SUPPLY: u64 = 1_000_000_000_000_000; // 1B tokens
pub const CURVE_TOKENS: u64 = 793_100_000_000_000; // 793.1M tokens sold on the curve
pub const INITIAL_VIRTUAL_TOKEN: u64 = 1_073_000_000_000_000;
pub const INITIAL_VIRTUAL_SOL: u64 = 2_000_000_000; // 2 SOL (virtual)
pub const FEE_BPS: u128 = 100; // 1% trade fee, paid to the token creator

#[program]
pub mod bonding_curve {
    use super::*;

    pub fn launch(
        ctx: Context<Launch>,
        name: String,
        symbol: String,
        uri: String,
    ) -> Result<()> {
        require!(name.len() > 0 && name.len() <= 32, CurveError::BadMeta);
        require!(symbol.len() > 0 && symbol.len() <= 10, CurveError::BadMeta);
        require!(uri.len() <= 200, CurveError::BadMeta);

        let curve = &mut ctx.accounts.curve;
        curve.creator = ctx.accounts.creator.key();
        curve.mint = ctx.accounts.mint.key();
        curve.name = name;
        curve.symbol = symbol;
        curve.uri = uri;
        curve.virtual_sol = INITIAL_VIRTUAL_SOL;
        curve.virtual_token = INITIAL_VIRTUAL_TOKEN;
        curve.real_sol = 0;
        curve.real_token = CURVE_TOKENS;
        curve.complete = false;
        curve.created_at = Clock::get()?.unix_timestamp;
        curve.bump = ctx.bumps.curve;

        let mint_key = ctx.accounts.mint.key();
        let seeds: &[&[u8]] = &[b"curve", mint_key.as_ref(), &[ctx.bumps.curve]];
        let signer = &[seeds];

        mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                MintTo {
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.curve.to_account_info(),
                },
                signer,
            ),
            TOTAL_SUPPLY,
        )?;
        // fix the supply forever
        set_authority(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                SetAuthority {
                    current_authority: ctx.accounts.curve.to_account_info(),
                    account_or_mint: ctx.accounts.mint.to_account_info(),
                },
                signer,
            ),
            AuthorityType::MintTokens,
            None,
        )?;

        emit!(LaunchEvent {
            mint: mint_key,
            creator: ctx.accounts.creator.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Spend up to `sol_amount` lamports (fee included) for at least `min_tokens_out` tokens.
    pub fn buy(ctx: Context<Trade>, sol_amount: u64, min_tokens_out: u64) -> Result<()> {
        let curve = &mut ctx.accounts.curve;
        require!(!curve.complete, CurveError::Complete);
        require!(sol_amount > 0, CurveError::ZeroAmount);

        let vs = curve.virtual_sol as u128;
        let vt = curve.virtual_token as u128;
        let k = vs * vt;

        let mut gross = sol_amount as u128;
        let mut fee = gross * FEE_BPS / 10_000;
        let mut net = gross - fee;
        let mut new_vs = vs + net;
        let mut new_vt = ceil_div(k, new_vs);
        let mut tokens_out = vt.checked_sub(new_vt).ok_or(CurveError::Math)?;

        if tokens_out >= curve.real_token as u128 {
            // last purchase: take what is left, charge only what it costs
            tokens_out = curve.real_token as u128;
            new_vt = vt - tokens_out;
            new_vs = ceil_div(k, new_vt);
            net = new_vs - vs;
            gross = ceil_div(net * 10_000, 10_000 - FEE_BPS);
            fee = gross - net;
            require!(gross <= sol_amount as u128, CurveError::Slippage);
        }
        require!(tokens_out > 0, CurveError::ZeroAmount);
        require!(tokens_out as u64 >= min_tokens_out, CurveError::Slippage);

        curve.virtual_sol = new_vs as u64;
        curve.virtual_token = new_vt as u64;
        curve.real_sol = curve.real_sol.checked_add(net as u64).ok_or(CurveError::Math)?;
        curve.real_token -= tokens_out as u64;
        if curve.real_token == 0 {
            curve.complete = true;
        }

        transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.trader.to_account_info(),
                    to: ctx.accounts.curve.to_account_info(),
                },
            ),
            net as u64,
        )?;
        transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.trader.to_account_info(),
                    to: ctx.accounts.creator.to_account_info(),
                },
            ),
            fee as u64,
        )?;

        let mint_key = ctx.accounts.mint.key();
        let bump = ctx.accounts.curve.bump;
        let seeds: &[&[u8]] = &[b"curve", mint_key.as_ref(), &[bump]];
        transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.trader_ata.to_account_info(),
                    authority: ctx.accounts.curve.to_account_info(),
                },
                &[seeds],
            ),
            tokens_out as u64,
            DECIMALS,
        )?;

        let c = &ctx.accounts.curve;
        emit!(TradeEvent {
            mint: mint_key,
            trader: ctx.accounts.trader.key(),
            is_buy: true,
            sol_amount: gross as u64,
            token_amount: tokens_out as u64,
            fee: fee as u64,
            virtual_sol: c.virtual_sol,
            virtual_token: c.virtual_token,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Sell `token_amount` tokens for at least `min_sol_out` lamports (after fee).
    pub fn sell(ctx: Context<Trade>, token_amount: u64, min_sol_out: u64) -> Result<()> {
        let curve = &mut ctx.accounts.curve;
        require!(!curve.complete, CurveError::Complete);
        require!(token_amount > 0, CurveError::ZeroAmount);

        let vs = curve.virtual_sol as u128;
        let vt = curve.virtual_token as u128;
        let k = vs * vt;
        let new_vt = vt + token_amount as u128;
        let new_vs = ceil_div(k, new_vt);
        let sol_out = vs.checked_sub(new_vs).ok_or(CurveError::Math)?;
        require!(sol_out > 0, CurveError::ZeroAmount);
        require!(sol_out <= curve.real_sol as u128, CurveError::Math);
        let fee = sol_out * FEE_BPS / 10_000;
        let to_trader = sol_out - fee;
        require!(to_trader as u64 >= min_sol_out, CurveError::Slippage);

        curve.virtual_sol = new_vs as u64;
        curve.virtual_token = new_vt as u64;
        curve.real_sol -= sol_out as u64;
        curve.real_token = curve.real_token.checked_add(token_amount).ok_or(CurveError::Math)?;

        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.trader_ata.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.trader.to_account_info(),
                },
            ),
            token_amount,
            DECIMALS,
        )?;

        // pay out of the curve's own lamports (program-owned account)
        let curve_info = ctx.accounts.curve.to_account_info();
        **curve_info.try_borrow_mut_lamports()? -= sol_out as u64;
        **ctx.accounts.trader.to_account_info().try_borrow_mut_lamports()? += to_trader as u64;
        **ctx.accounts.creator.to_account_info().try_borrow_mut_lamports()? += fee as u64;

        let c = &ctx.accounts.curve;
        emit!(TradeEvent {
            mint: ctx.accounts.mint.key(),
            trader: ctx.accounts.trader.key(),
            is_buy: false,
            sol_amount: to_trader as u64,
            token_amount,
            fee: fee as u64,
            virtual_sol: c.virtual_sol,
            virtual_token: c.virtual_token,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }
}

fn ceil_div(a: u128, b: u128) -> u128 {
    (a + b - 1) / b
}

#[derive(Accounts)]
pub struct Launch<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(
        init,
        payer = creator,
        mint::decimals = DECIMALS,
        mint::authority = curve,
        mint::token_program = token_program,
    )]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = creator,
        space = 8 + Curve::INIT_SPACE,
        seeds = [b"curve", mint.key().as_ref()],
        bump,
    )]
    pub curve: Account<'info, Curve>,
    #[account(
        init,
        payer = creator,
        associated_token::mint = mint,
        associated_token::authority = curve,
        associated_token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Trade<'info> {
    #[account(mut)]
    pub trader: Signer<'info>,
    #[account(
        mut,
        seeds = [b"curve", mint.key().as_ref()],
        bump = curve.bump,
        has_one = mint,
        has_one = creator,
    )]
    pub curve: Account<'info, Curve>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = curve,
        associated_token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = trader,
        associated_token::mint = mint,
        associated_token::authority = trader,
        associated_token::token_program = token_program,
    )]
    pub trader_ata: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: receives the 1% fee; must equal curve.creator (has_one)
    #[account(mut)]
    pub creator: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[account]
#[derive(InitSpace)]
pub struct Curve {
    pub creator: Pubkey,
    pub mint: Pubkey,
    #[max_len(32)]
    pub name: String,
    #[max_len(10)]
    pub symbol: String,
    #[max_len(200)]
    pub uri: String,
    pub virtual_sol: u64,
    pub virtual_token: u64,
    pub real_sol: u64,
    pub real_token: u64,
    pub complete: bool,
    pub created_at: i64,
    pub bump: u8,
}

#[event]
pub struct LaunchEvent {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct TradeEvent {
    pub mint: Pubkey,
    pub trader: Pubkey,
    pub is_buy: bool,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub fee: u64,
    pub virtual_sol: u64,
    pub virtual_token: u64,
    pub timestamp: i64,
}

#[error_code]
pub enum CurveError {
    #[msg("Name, symbol or uri is empty or too long")]
    BadMeta,
    #[msg("The curve is complete, trading has ended")]
    Complete,
    #[msg("Amount must be above zero")]
    ZeroAmount,
    #[msg("Price moved past your slippage limit")]
    Slippage,
    #[msg("Math error")]
    Math,
}
