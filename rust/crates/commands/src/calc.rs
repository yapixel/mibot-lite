use crate::kit;
use mibot_app::App;
use mibot_command::{Command, Invocation, code, group};
use std::sync::Arc;

const MAX_EXPRESSION_LENGTH: usize = 120;

struct CalcParser {
    text: Vec<char>,
    index: usize,
}

impl CalcParser {
    fn new(expr: &str) -> Self {
        Self {
            text: expr.chars().collect(),
            index: 0,
        }
    }

    fn parse(&mut self) -> Result<f64, &'static str> {
        let value = self.additive()?;
        self.skip_space();
        if self.index != self.text.len() {
            return Err(kit::tr("Invalid expression format", "表达式格式错误"));
        }
        const MAX_SAFE_INT: f64 = ((1u64 << 53) - 1) as f64;
        if value.is_infinite() || value.is_nan() || value.abs() > MAX_SAFE_INT {
            return Err(kit::tr("Result out of safe range", "计算结果超出安全范围"));
        }
        Ok(value)
    }

    fn additive(&mut self) -> Result<f64, &'static str> {
        let mut value = self.multiplicative()?;
        loop {
            self.skip_space();
            let op = self.peek();
            if op != '+' && op != '-' {
                return Ok(value);
            }
            self.index += 1;
            let right = self.multiplicative()?;
            if op == '+' {
                value += right;
            } else {
                value -= right;
            }
        }
    }

    fn multiplicative(&mut self) -> Result<f64, &'static str> {
        let mut value = self.unary()?;
        loop {
            self.skip_space();
            let op = self.peek();
            if op != '*' && op != '/' {
                return Ok(value);
            }
            self.index += 1;
            let right = self.unary()?;
            if op == '/' && right == 0.0 {
                return Err(kit::tr("Division by zero", "除零错误"));
            }
            if op == '*' {
                value *= right;
            } else {
                value /= right;
            }
            if value.is_infinite() || value.is_nan() {
                return Err(kit::tr("Invalid calculation result", "计算结果无效"));
            }
        }
    }

    fn unary(&mut self) -> Result<f64, &'static str> {
        self.skip_space();
        let op = self.peek();
        if op == '+' || op == '-' {
            self.index += 1;
            let value = self.unary()?;
            return Ok(if op == '-' { -value } else { value });
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<f64, &'static str> {
        self.skip_space();
        if self.peek() == '(' {
            self.index += 1;
            let value = self.additive()?;
            self.skip_space();
            if self.peek() != ')' {
                return Err(kit::tr("Unmatched parentheses", "括号不匹配"));
            }
            self.index += 1;
            return Ok(value);
        }
        let start = self.index;
        while self.index < self.text.len()
            && (self.text[self.index].is_ascii_digit() || self.text[self.index] == '.')
        {
            self.index += 1;
        }
        let token: String = self.text[start..self.index].iter().collect();
        if !number_token(&token) {
            return Err(kit::tr("Invalid number format", "数字格式错误"));
        }
        let value = token
            .parse::<f64>()
            .map_err(|_| kit::tr("Number out of range", "数字超出范围"))?;
        if value.is_infinite() {
            return Err(kit::tr("Number out of range", "数字超出范围"));
        }
        Ok(value)
    }

    fn peek(&self) -> char {
        self.text.get(self.index).copied().unwrap_or('\0')
    }

    fn skip_space(&mut self) {
        while self.index < self.text.len()
            && matches!(self.text[self.index], ' ' | '\t' | '\n' | '\r')
        {
            self.index += 1;
        }
    }
}

fn number_token(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    match token.split_once('.') {
        Some((whole, fraction)) => {
            !whole.is_empty()
                && !fraction.is_empty()
                && whole.bytes().all(|b| b.is_ascii_digit())
                && fraction.bytes().all(|b| b.is_ascii_digit())
        }
        None => token.bytes().all(|b| b.is_ascii_digit()),
    }
}

pub fn format_calc(value: f64) -> String {
    if value == value.trunc() {
        if value == 0.0 {
            return "0".to_owned();
        }
        return format!("{value:.0}");
    }
    let rounded = round_half_up(value * 1e12) / 1e12;
    if rounded == 0.0 {
        return "0".to_owned();
    }
    js_number(rounded)
}

fn round_half_up(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

fn js_number(value: f64) -> String {
    let text = format!("{value:e}");
    let (sign, rest) = match text.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", text.as_str()),
    };
    let (mantissa, exp_str) = rest.split_once('e').unwrap_or((rest, "0"));
    let power: i32 = exp_str.parse().unwrap_or(0);
    let digits = mantissa.replace('.', "");
    let k = digits.len() as i32;
    let n = power + 1;
    if k <= n && n <= 21 {
        return format!("{sign}{digits}{}", "0".repeat((n - k) as usize));
    }
    if 0 < n && n <= 21 {
        let split = n as usize;
        return format!("{sign}{}.{}", &digits[..split], &digits[split..]);
    }
    if -6 < n && n <= 0 {
        return format!("{sign}0.{}{digits}", "0".repeat((-n) as usize));
    }
    let (exp_sign, abs_pow) = if power < 0 {
        ("-", -power)
    } else {
        ("+", power)
    };
    if k == 1 {
        format!("{sign}{digits}e{exp_sign}{abs_pow}")
    } else {
        format!("{sign}{}.{}e{exp_sign}{abs_pow}", &digits[..1], &digits[1..])
    }
}

fn help(prefix: &str) -> String {
    let note = kit::tr(
        "Supports parentheses, decimals, and negative numbers.",
        "支持括号、小数和负数。",
    );
    format!(
        "🧮 <b>{}</b>\n\n• {}\n• {}\n• {}\n{note}",
        kit::tr("Calculator", "计算器"),
        code(&format!("{prefix}calc 2+2*5")),
        code(&format!("{prefix}calc (10-3)*4")),
        code(&format!("{prefix}calc -(2-5)/3")),
    )
}

pub fn register(a: &Arc<App>) {
    let mut cmd: Command = kit::make_cmd(
        "calc",
        &[],
        group::TOOLS,
        "Arithmetic calculator",
        "expression",
        help,
        move |inv: Invocation| {
            Box::pin(async move {
                let expression = inv.raw.trim();
                let lower = expression.to_ascii_lowercase();
                if expression.is_empty() || lower == "help" || lower == "h" {
                    return inv.edit(&help(inv.prefix())).await.map_err(Into::into);
                }
                if expression.chars().count() > MAX_EXPRESSION_LENGTH {
                    return Err(kit::fail_err(if kit::is_zh() {
                        format!("计算失败：表达式不能超过 {MAX_EXPRESSION_LENGTH} 个字符")
                    } else {
                        format!(
                            "Calculation failed: expression cannot exceed {MAX_EXPRESSION_LENGTH} characters"
                        )
                    }));
                }
                let mut parser = CalcParser::new(expression);
                let result = parser.parse().map_err(|msg| {
                    kit::fail_err(format!(
                        "{}{msg}",
                        kit::tr("Calculation failed: ", "计算失败：")
                    ))
                })?;
                inv.edit(&format!(
                    "🧮 <b>{}</b>\n{} = {}",
                    kit::tr("Result", "计算结果"),
                    code(expression),
                    code(&format_calc(result))
                ))
                .await
                .map_err(Into::into)
            })
        },
    )
    .zh("计算四则运算表达式", "表达式");
    cmd.free_text = true;
    a.registry.register(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calc_evaluates() {
        let cases = [
            ("2+2*5", "12"),
            ("(10-3)*4", "28"),
            ("-(2-5)/3", "1"),
            ("3+7", "10"),
            ("8/2+5", "9"),
            ("1.5*2", "3"),
            ("10/4", "2.5"),
            ("  7  -  2  ", "5"),
        ];
        for (expr, want) in cases {
            let v = CalcParser::new(expr).parse().expect(expr);
            assert_eq!(format_calc(v), want, "{expr}");
        }
    }

    #[test]
    fn test_calc_formats_like_mibox() {
        let cases = [
            ("100000/3", "33333.333333333336"),
            ("1/3", "0.333333333333"),
            ("2/3", "0.666666666667"),
            ("-1/3", "-0.333333333333"),
            ("0.1+0.2", "0.3"),
            ("0.1*3", "0.3"),
            ("0.0000001*1", "1e-7"),
            ("0.000001*1", "0.000001"),
            ("0.00000012345*1", "1.2345e-7"),
            ("-0.0000000000025*1", "-2e-12"),
            ("0.0000000000004*1", "0"),
            ("1000000000000000.5", "1000000000000000.6"),
            ("9007199254740991", "9007199254740991"),
            ("1234567.891", "1234567.891"),
            ("22/7", "3.142857142857"),
            ("-0", "0"),
            ("5000000000000*3", "15000000000000"),
            ("1/7", "0.142857142857"),
            ("123456789.123456789", "123456789.12345679"),
        ];
        for (expr, want) in cases {
            let v = CalcParser::new(expr).parse().expect(expr);
            assert_eq!(format_calc(v), want, "{expr}");
        }
    }

    #[test]
    fn test_calc_rejects() {
        for expr in [
            "1/0", "(1+2", "1++", "2**3", "abc", "", "1 2", "0x10", "1.2.3",
        ] {
            assert!(CalcParser::new(expr).parse().is_err(), "{expr:?} should fail");
        }
    }
}
