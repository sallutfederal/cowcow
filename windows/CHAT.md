# Chave da Anthropic para o chat

O Claude e o OpenAI leem a chave do Gerenciador de Credenciais do Windows, nunca de um arquivo.
Sem a chave, a ilha continua funcionando — só o chat responde que não achou a credencial.

No PowerShell:

```powershell
$key = Read-Host "anthropic-api-key" -AsSecureString
$ptr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($key)
try { $plain = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr)
      cmdkey /generic:anthropic-api-key /user:anthropic /pass:$plain | Out-Null }
finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr) }
```

Verificar, sem imprimir a chave:

```powershell
cmdkey /list:anthropic-api-key
```

Remover: `cmdkey /delete:anthropic-api-key`

O provedor precisa estar em `claude` na janela de ajustes, com o modelo preenchido.

## Aceitar de ponta a ponta

```powershell
cd windows
npm run build
cargo build --manifest-path src-tauri\Cargo.toml --release
```

1. Abrir o app, 2. perguntar algo, 3. fechar, 4. reabrir, 5. perguntar de novo.
   A segunda pergunta precisa carregar a primeira.
6. Arrastar um arquivo para a ilha: isso começa uma conversa nova e a anterior continua
   encontrável por `chat_search`.

## Testes

```powershell
cargo test --manifest-path src-tauri\Cargo.toml
```

O teste de persistência sobe um servidor falso na porta que o Windows escolher e responde
como um provider OpenAI-compatible, então ele roda sem chave e sem gastar token.