#include "Luau/Ast.h"
#include "Luau/BuiltinDefinitions.h"
#include "Luau/Config.h"
#include "Luau/ConfigResolver.h"
#include "Luau/Error.h"
#include "Luau/FileResolver.h"
#include "Luau/Frontend.h"
#include "Luau/Parser.h"
#include "lua.h"

#include <algorithm>
#include <cctype>
#include <cstdint>
#include <exception>
#include <filesystem>
#include <fstream>
#include <memory>
#include <optional>
#include <sstream>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

extern "C" {

#ifndef RUAU_ANALYZE_EXPORT
#define RUAU_ANALYZE_EXPORT
#endif

struct RuauDiagnostic
{
    uint32_t line;
    uint32_t col;
    uint32_t end_line;
    uint32_t end_col;
    uint32_t severity; // 0: error, 1: warning
    const char* message;
    uint32_t message_len;
};

struct RuauCheckResult
{
    void* _internal;
    const RuauDiagnostic* diagnostics;
    uint32_t diagnostic_count;
    uint32_t timed_out;
    uint32_t cancelled;
};

struct RuauEntrypointParam
{
    const char* name;
    uint32_t name_len;
    const char* annotation;
    uint32_t annotation_len;
    uint32_t optional;
};

struct RuauEntrypointSchemaResult
{
    void* _internal;
    const RuauEntrypointParam* params;
    uint32_t param_count;
    const char* error;
    uint32_t error_len;
};

struct RuauString
{
    void* _internal;
    const char* data;
    uint32_t len;
};

typedef struct RuauCancellationToken RuauCancellationToken;
struct RuauVirtualModule;

struct RuauCheckOptions
{
    const char* module_name;
    uint32_t module_name_len;
    uint32_t has_timeout;
    double timeout_seconds;
    RuauCancellationToken* cancellation_token;
    const RuauVirtualModule* virtual_modules;
    uint32_t virtual_module_count;
};

typedef struct RuauChecker RuauChecker;

struct RuauVirtualModule
{
    const char* name;
    uint32_t name_len;
    const char* source;
    uint32_t source_len;
};

RUAU_ANALYZE_EXPORT RuauChecker* ruau_checker_new(void);
RUAU_ANALYZE_EXPORT void ruau_checker_free(RuauChecker* checker);

RUAU_ANALYZE_EXPORT RuauCancellationToken* ruau_cancellation_token_new(void);
RUAU_ANALYZE_EXPORT void ruau_cancellation_token_free(RuauCancellationToken* token);
RUAU_ANALYZE_EXPORT void ruau_cancellation_token_cancel(RuauCancellationToken* token);
RUAU_ANALYZE_EXPORT void ruau_cancellation_token_reset(RuauCancellationToken* token);

RUAU_ANALYZE_EXPORT RuauString ruau_checker_add_definitions(
    RuauChecker* checker,
    const char* defs,
    uint32_t defs_len,
    const char* module_name,
    uint32_t module_name_len
);
RUAU_ANALYZE_EXPORT RuauCheckResult ruau_checker_check(
    RuauChecker* checker,
    const char* source,
    uint32_t source_len,
    const RuauCheckOptions* options
);
RUAU_ANALYZE_EXPORT RuauEntrypointSchemaResult ruau_extract_entrypoint_schema(
    const char* source,
    uint32_t source_len
);

RUAU_ANALYZE_EXPORT void ruau_check_result_free(RuauCheckResult result);
RUAU_ANALYZE_EXPORT void ruau_entrypoint_schema_result_free(RuauEntrypointSchemaResult result);
RUAU_ANALYZE_EXPORT void ruau_string_free(RuauString value);
}

namespace
{

uint32_t as_u32(size_t value)
{
    return value > UINT32_MAX ? UINT32_MAX : static_cast<uint32_t>(value);
}

std::optional<std::string> read_text_file(const std::filesystem::path& path)
{
    std::ifstream stream(path, std::ios::binary);
    if (!stream)
        return std::nullopt;

    std::ostringstream buffer;
    buffer << stream.rdbuf();
    return buffer.str();
}

std::filesystem::path normalize_path(std::filesystem::path path)
{
    return path.lexically_normal();
}

std::optional<std::filesystem::path> resolve_file_module(const std::filesystem::path& path)
{
    std::optional<std::filesystem::path> found;

    auto try_candidate = [&found](std::filesystem::path candidate) -> bool
    {
        if (!std::filesystem::is_regular_file(candidate))
            return false;
        if (found)
            return false;
        found = normalize_path(std::move(candidate));
        return true;
    };

    if (path.filename() != "init")
    {
        const std::string currentExtension = path.has_extension() ? path.extension().string().substr(1) + "." : "";
        try_candidate(path.string() + "." + currentExtension + "luau");
        try_candidate(path.string() + "." + currentExtension + "lua");
    }

    if (std::filesystem::is_directory(path))
    {
        try_candidate(path / "init.luau");
        try_candidate(path / "init.lua");
    }

    return found;
}

struct GraphFileResolver : Luau::FileResolver
{
    std::unordered_map<Luau::ModuleName, std::string> rootSources;
    std::unordered_map<Luau::ModuleName, std::string> virtualSources;

    void reset()
    {
        rootSources.clear();
        virtualSources.clear();
    }

    void setRootSource(const Luau::ModuleName& name, std::string source)
    {
        rootSources[name] = std::move(source);
    }

    void addVirtualSource(const Luau::ModuleName& name, std::string source)
    {
        virtualSources[name] = std::move(source);
    }

    std::optional<Luau::SourceCode> readSource(const Luau::ModuleName& name) override
    {
        if (auto iterator = rootSources.find(name); iterator != rootSources.end())
            return Luau::SourceCode{iterator->second, Luau::SourceCode::Module};
        if (auto iterator = virtualSources.find(name); iterator != virtualSources.end())
            return Luau::SourceCode{iterator->second, Luau::SourceCode::Module};
        if (auto source = read_text_file(name))
            return Luau::SourceCode{*source, Luau::SourceCode::Module};
        return std::nullopt;
    }

    std::optional<Luau::ModuleInfo> resolveModule(
        const Luau::ModuleInfo* context,
        Luau::AstExpr* expr,
        const Luau::TypeCheckLimits& limits
    ) override
    {
        if (const auto* global = expr->as<Luau::AstExprGlobal>())
            return resolveNamedModule(global->name.value);
        if (const auto* string = expr->as<Luau::AstExprConstantString>())
        {
            std::string path(string->value.data, string->value.size);
            if (auto module = resolveNamedModule(path))
                return module;
            return resolveFilesystemModule(context, std::move(path), limits);
        }
        return std::nullopt;
    }

    std::string getHumanReadableModuleName(const Luau::ModuleName& name) const override
    {
        return name;
    }

    std::optional<std::string> getEnvironmentForModule(const Luau::ModuleName&) const override
    {
        return std::nullopt;
    }

private:
    std::optional<Luau::ModuleInfo> resolveNamedModule(const std::string& name) const
    {
        if (virtualSources.find(name) != virtualSources.end() || rootSources.find(name) != rootSources.end())
            return Luau::ModuleInfo{name};
        return std::nullopt;
    }

    std::optional<Luau::ModuleInfo> resolveFilesystemModule(
        const Luau::ModuleInfo* context,
        std::string path,
        const Luau::TypeCheckLimits& limits
    ) const
    {
        if (context == nullptr)
            return std::nullopt;

        (void)limits;

        std::filesystem::path base(context->name);
        if (base.has_filename())
            base = base.parent_path();

        std::filesystem::path requested(path);
        std::filesystem::path candidate = requested.is_absolute() ? requested : base / requested;
        if (auto resolved = resolve_file_module(normalize_path(candidate)))
            return Luau::ModuleInfo{resolved->string()};
        return resolveNamedModule(path);
    }
};

struct StrictConfigResolver : Luau::ConfigResolver
{
    StrictConfigResolver()
    {
        strictConfig.mode = Luau::Mode::Strict;
    }

    const Luau::Config& getConfig(const Luau::ModuleName&, const Luau::TypeCheckLimits&) const override
    {
        return strictConfig;
    }

    Luau::Config strictConfig;
};

struct DiagnosticEntry
{
    RuauDiagnostic diagnostic{};
    std::string message;
};

struct CheckResultStorage
{
    std::vector<DiagnosticEntry> entries;
    std::vector<RuauDiagnostic> diagnostics;
    bool timedOut = false;
    bool cancelled = false;
};

struct StringStorage
{
    std::string value;
};

struct EntrypointParamStorage
{
    RuauEntrypointParam param{};
    std::string name;
    std::string annotation;
};

struct EntrypointSchemaStorage
{
    std::vector<EntrypointParamStorage> entries;
    std::vector<RuauEntrypointParam> params;
    std::string error;
};

struct CheckerImpl
{
    GraphFileResolver fileResolver;
    StrictConfigResolver configResolver;
    Luau::FrontendOptions frontendOptions;
    std::unique_ptr<Luau::Frontend> frontend;

    CheckerImpl()
    {
        frontendOptions.retainFullTypeGraphs = false;
        frontendOptions.runLintChecks = true;
        frontend = std::make_unique<Luau::Frontend>(&fileResolver, &configResolver, frontendOptions);
        frontend->setLuauSolverMode(Luau::SolverMode::New);

        Luau::unfreeze(frontend->globals.globalTypes);
        Luau::registerBuiltinGlobals(*frontend, frontend->globals);
        Luau::freeze(frontend->globals.globalTypes);
    }
};

std::vector<size_t> line_starts(const std::string& source)
{
    std::vector<size_t> starts;
    starts.push_back(0);
    for (size_t index = 0; index < source.size(); ++index)
    {
        if (source[index] == '\n')
            starts.push_back(index + 1);
    }
    return starts;
}

size_t offset_for_position(const std::vector<size_t>& starts, const Luau::Position& position, size_t sourceLen)
{
    if (position.line >= starts.size())
        return sourceLen;
    return std::min(starts[position.line] + position.column, sourceLen);
}

std::string source_slice(const std::string& source, const Luau::Location& location)
{
    const std::vector<size_t> starts = line_starts(source);
    const size_t begin = offset_for_position(starts, location.begin, source.size());
    const size_t end = offset_for_position(starts, location.end, source.size());
    if (begin >= end || begin >= source.size())
        return std::string();
    return source.substr(begin, end - begin);
}

std::string trim_copy(std::string value)
{
    auto isSpace = [](unsigned char ch) { return std::isspace(ch) != 0; };
    auto begin = std::find_if_not(value.begin(), value.end(), isSpace);
    auto end = std::find_if_not(value.rbegin(), value.rend(), isSpace).base();
    if (begin >= end)
        return std::string();
    return std::string(begin, end);
}

bool annotation_is_optional(const std::string& annotation)
{
    const std::string trimmed = trim_copy(annotation);
    return !trimmed.empty() && trimmed.back() == '?';
}

std::string parse_error_message(const Luau::ParseError& error)
{
    std::string message;
    message += "main:";
    message += std::to_string(error.getLocation().begin.line + 1);
    message += ":";
    message += std::to_string(error.getLocation().begin.column + 1);
    message += ": ";
    message += error.getMessage();
    return message;
}

EntrypointSchemaStorage* extract_entrypoint_schema_storage(const std::string& source)
{
    auto* storage = new EntrypointSchemaStorage();

    try
    {
        Luau::Allocator allocator;
        Luau::AstNameTable names(allocator);
        Luau::ParseOptions options;
        options.allowDeclarationSyntax = true;

        Luau::ParseResult parseResult = Luau::Parser::parse(
            source.data(),
            source.size(),
            names,
            allocator,
            std::move(options)
        );

        if (!parseResult.errors.empty())
        {
            storage->error = parse_error_message(parseResult.errors.front());
            return storage;
        }

        Luau::AstStatBlock* root = parseResult.root;
        if (root == nullptr || root->body.size != 1)
        {
            storage->error = "script must use a direct `return function(...) ... end` entrypoint";
            return storage;
        }

        const auto* ret = root->body.data[0]->as<Luau::AstStatReturn>();
        if (ret == nullptr || ret->list.size != 1)
        {
            storage->error = "script must use a direct `return function(...) ... end` entrypoint";
            return storage;
        }

        const auto* func = ret->list.data[0]->as<Luau::AstExprFunction>();
        if (func == nullptr)
        {
            storage->error = "script must use a direct `return function(...) ... end` entrypoint";
            return storage;
        }

        if (func->self != nullptr)
        {
            storage->error = "entrypoint functions may not use a self parameter";
            return storage;
        }

        if (func->vararg)
        {
            storage->error = "entrypoint functions may not be variadic";
            return storage;
        }

        for (const Luau::AstLocal* local : func->args)
        {
            if (local == nullptr || local->name.value == nullptr)
            {
                storage->error = "entrypoint parameters must be named";
                return storage;
            }
            if (local->annotation == nullptr)
            {
                storage->error = std::string("parameter `") + local->name.value + "` is missing a type annotation";
                return storage;
            }

            EntrypointParamStorage entry;
            entry.name = local->name.value;
            entry.annotation = trim_copy(source_slice(source, local->annotation->location));
            if (entry.annotation.empty())
            {
                storage->error = std::string("parameter `") + local->name.value + "` is missing a type annotation";
                return storage;
            }

            entry.param.name = entry.name.c_str();
            entry.param.name_len = as_u32(entry.name.size());
            entry.param.annotation = entry.annotation.c_str();
            entry.param.annotation_len = as_u32(entry.annotation.size());
            entry.param.optional = annotation_is_optional(entry.annotation) ? 1u : 0u;
            storage->entries.push_back(std::move(entry));
        }

        storage->params.reserve(storage->entries.size());
        for (auto& entry : storage->entries)
        {
            entry.param.name = entry.name.c_str();
            entry.param.annotation = entry.annotation.c_str();
            storage->params.push_back(entry.param);
        }
    }
    catch (const std::exception& error)
    {
        storage->error = error.what();
    }
    catch (...)
    {
        storage->error = "unknown internal entrypoint schema extraction error";
    }

    return storage;
}

} // namespace

struct RuauChecker
{
    CheckerImpl* impl;
};

struct RuauCancellationToken
{
    std::shared_ptr<Luau::FrontendCancellationToken> token;
};

namespace
{

RuauString make_luau_string(std::string message)
{
    if (message.empty())
        return RuauString{nullptr, nullptr, 0};

    auto* storage = new StringStorage{std::move(message)};
    return RuauString{storage, storage->value.c_str(), as_u32(storage->value.size())};
}

void push_diagnostic(
    CheckResultStorage& storage,
    uint32_t line,
    uint32_t col,
    uint32_t endLine,
    uint32_t endCol,
    uint32_t severity,
    std::string message
)
{
    DiagnosticEntry entry;
    entry.diagnostic.line = line;
    entry.diagnostic.col = col;
    entry.diagnostic.end_line = endLine;
    entry.diagnostic.end_col = endCol;
    entry.diagnostic.severity = severity;
    entry.message = std::move(message);
    storage.entries.push_back(std::move(entry));
}

void push_type_error(CheckResultStorage& storage, const Luau::TypeError& error)
{
    push_diagnostic(
        storage,
        as_u32(error.location.begin.line),
        as_u32(error.location.begin.column),
        as_u32(error.location.end.line),
        as_u32(error.location.end.column),
        0,
        Luau::toString(error)
    );
}

void push_lint(CheckResultStorage& storage, const Luau::LintWarning& warning, uint32_t severity)
{
    push_diagnostic(
        storage,
        as_u32(warning.location.begin.line),
        as_u32(warning.location.begin.column),
        as_u32(warning.location.end.line),
        as_u32(warning.location.end.column),
        severity,
        warning.text
    );
}

void push_internal_error(CheckResultStorage& storage, const std::string& message)
{
    push_diagnostic(storage, 0, 0, 0, 0, 0, message);
}

std::string to_owned_string(const char* data, uint32_t len)
{
    if (data == nullptr || len == 0)
        return {};
    return std::string(data, len);
}

std::string fallback_label(const std::string& value, const char* fallback)
{
    return value.empty() ? std::string(fallback) : value;
}

void populate_virtual_modules(GraphFileResolver& fileResolver, const RuauCheckOptions* options)
{
    if (options == nullptr || options->virtual_modules == nullptr || options->virtual_module_count == 0)
        return;

    for (uint32_t index = 0; index < options->virtual_module_count; ++index)
    {
        const RuauVirtualModule& module = options->virtual_modules[index];
        const std::string moduleName = to_owned_string(module.name, module.name_len);
        fileResolver.addVirtualSource(moduleName, to_owned_string(module.source, module.source_len));
    }
}

std::string definitions_error_message(const Luau::LoadDefinitionFileResult& result, const std::string& moduleName)
{
    std::string message;

    for (const auto& parseError : result.parseResult.errors)
    {
        if (!message.empty())
            message += "\n";
        message += moduleName;
        message += ":";
        message += std::to_string(parseError.getLocation().begin.line + 1);
        message += ":";
        message += std::to_string(parseError.getLocation().begin.column + 1);
        message += ": ";
        message += parseError.getMessage();
    }

    if (result.module)
    {
        for (const auto& error : result.module->errors)
        {
            if (!message.empty())
                message += "\n";
            message += moduleName;
            message += ":";
            message += std::to_string(error.location.begin.line + 1);
            message += ":";
            message += std::to_string(error.location.begin.column + 1);
            message += ": ";
            message += Luau::toString(error);
        }
    }

    if (message.empty())
        message = "failed to load Luau definitions";
    return message;
}

RuauCheckResult finalize_check_result(CheckResultStorage* storage)
{
    std::sort(
        storage->entries.begin(),
        storage->entries.end(),
        [](const DiagnosticEntry& left, const DiagnosticEntry& right)
        {
            if (left.diagnostic.line != right.diagnostic.line)
                return left.diagnostic.line < right.diagnostic.line;
            if (left.diagnostic.col != right.diagnostic.col)
                return left.diagnostic.col < right.diagnostic.col;
            if (left.diagnostic.severity != right.diagnostic.severity)
                return left.diagnostic.severity < right.diagnostic.severity;
            return left.message < right.message;
        }
    );

    storage->diagnostics.reserve(storage->entries.size());
    for (auto& entry : storage->entries)
    {
        entry.diagnostic.message = entry.message.c_str();
        entry.diagnostic.message_len = as_u32(entry.message.size());
        storage->diagnostics.push_back(entry.diagnostic);
    }

    return RuauCheckResult{
        storage,
        storage->diagnostics.data(),
        as_u32(storage->diagnostics.size()),
        storage->timedOut ? 1u : 0u,
        storage->cancelled ? 1u : 0u,
    };
}

} // namespace

extern "C" RuauChecker* ruau_checker_new(void)
{
    try
    {
        auto* checker = new RuauChecker();
        checker->impl = new CheckerImpl();
        return checker;
    }
    catch (...)
    {
        return nullptr;
    }
}

extern "C" void ruau_checker_free(RuauChecker* checker)
{
    if (checker == nullptr)
        return;
    delete checker->impl;
    delete checker;
}

extern "C" RuauCancellationToken* ruau_cancellation_token_new(void)
{
    try
    {
        auto* token = new RuauCancellationToken();
        token->token = std::make_shared<Luau::FrontendCancellationToken>();
        token->token->cancelled.store(false);
        return token;
    }
    catch (...)
    {
        return nullptr;
    }
}

extern "C" void ruau_cancellation_token_free(RuauCancellationToken* token)
{
    delete token;
}

extern "C" void ruau_cancellation_token_cancel(RuauCancellationToken* token)
{
    if (token == nullptr || !token->token)
        return;
    token->token->cancel();
}

extern "C" void ruau_cancellation_token_reset(RuauCancellationToken* token)
{
    if (token == nullptr || !token->token)
        return;
    token->token->cancelled.store(false);
}

extern "C" RuauString ruau_checker_add_definitions(
    RuauChecker* checker,
    const char* defs,
    uint32_t defs_len,
    const char* module_name,
    uint32_t module_name_len
)
{
    if (checker == nullptr)
        return make_luau_string("checker is null");
    if (defs == nullptr && defs_len > 0)
        return make_luau_string("definitions pointer is null");

    try
    {
        const std::string source = defs == nullptr ? std::string() : std::string(defs, defs_len);
        const std::string moduleName = fallback_label(to_owned_string(module_name, module_name_len), "@definitions");

        Luau::unfreeze(checker->impl->frontend->globals.globalTypes);
        Luau::LoadDefinitionFileResult result = checker->impl->frontend->loadDefinitionFile(
            checker->impl->frontend->globals,
            checker->impl->frontend->globals.globalScope,
            source,
            moduleName,
            false,
            false
        );
        Luau::freeze(checker->impl->frontend->globals.globalTypes);

        if (result.success)
            return RuauString{nullptr, nullptr, 0};
        return make_luau_string(definitions_error_message(result, moduleName));
    }
    catch (const std::exception& error)
    {
        return make_luau_string(error.what());
    }
    catch (...)
    {
        return make_luau_string("unknown error while adding definitions");
    }
}

extern "C" RuauCheckResult ruau_checker_check(
    RuauChecker* checker,
    const char* source,
    uint32_t source_len,
    const RuauCheckOptions* options
)
{
    auto* storage = new CheckResultStorage();

    if (checker == nullptr)
    {
        push_internal_error(*storage, "checker is null");
        return finalize_check_result(storage);
    }
    if (source == nullptr && source_len > 0)
    {
        push_internal_error(*storage, "source pointer is null");
        return finalize_check_result(storage);
    }

    try
    {
        std::string moduleName = "main";
        Luau::FrontendOptions frontendOptions = checker->impl->frontendOptions;

        if (options != nullptr)
        {
            moduleName = fallback_label(to_owned_string(options->module_name, options->module_name_len), "main");
            if (options->has_timeout != 0 && options->timeout_seconds >= 0.0)
                frontendOptions.moduleTimeLimitSec = options->timeout_seconds;
            if (options->cancellation_token != nullptr)
                frontendOptions.cancellationToken = options->cancellation_token->token;
        }

        checker->impl->frontend->clear();
        checker->impl->fileResolver.reset();
        checker->impl->fileResolver.setRootSource(
            moduleName,
            source == nullptr ? std::string() : std::string(source, source_len)
        );
        populate_virtual_modules(checker->impl->fileResolver, options);

        Luau::CheckResult checkResult = checker->impl->frontend->check(moduleName, frontendOptions);
        for (const auto& error : checkResult.errors)
            push_type_error(*storage, error);
        for (const auto& error : checkResult.lintResult.errors)
            push_lint(*storage, error, 0);
        for (const auto& warning : checkResult.lintResult.warnings)
            push_lint(*storage, warning, 1);

        for (const auto& timeoutHit : checkResult.timeoutHits)
        {
            storage->timedOut = true;
            push_internal_error(*storage, "type checking timed out for module `" + timeoutHit + "`");
        }

        if (options != nullptr && options->cancellation_token != nullptr && options->cancellation_token->token &&
            options->cancellation_token->token->requested() && checkResult.errors.empty() &&
            checkResult.lintResult.errors.empty() && checkResult.lintResult.warnings.empty())
        {
            storage->cancelled = true;
            push_internal_error(*storage, "analysis has been cancelled");
        }
        else if (options != nullptr && options->cancellation_token != nullptr && options->cancellation_token->token &&
            options->cancellation_token->token->requested())
            storage->cancelled = true;
    }
    catch (const std::exception& error)
    {
        push_internal_error(*storage, error.what());
    }
    catch (...)
    {
        push_internal_error(*storage, "unknown internal checker error");
    }

    return finalize_check_result(storage);
}

extern "C" RuauEntrypointSchemaResult ruau_extract_entrypoint_schema(
    const char* source,
    uint32_t source_len
)
{
    if (source == nullptr && source_len > 0)
    {
        auto* storage = new EntrypointSchemaStorage();
        storage->error = "source pointer is null";
        return RuauEntrypointSchemaResult{
            storage,
            nullptr,
            0,
            storage->error.c_str(),
            as_u32(storage->error.size()),
        };
    }

    const std::string ownedSource =
        source == nullptr ? std::string() : std::string(source, source_len);
    EntrypointSchemaStorage* storage = extract_entrypoint_schema_storage(ownedSource);
    return RuauEntrypointSchemaResult{
        storage,
        storage->params.data(),
        as_u32(storage->params.size()),
        storage->error.empty() ? nullptr : storage->error.c_str(),
        as_u32(storage->error.size()),
    };
}

extern "C" void ruau_check_result_free(RuauCheckResult result)
{
    delete static_cast<CheckResultStorage*>(result._internal);
}

extern "C" void ruau_entrypoint_schema_result_free(RuauEntrypointSchemaResult result)
{
    delete static_cast<EntrypointSchemaStorage*>(result._internal);
}

extern "C" void ruau_string_free(RuauString value)
{
    delete static_cast<StringStorage*>(value._internal);
}
