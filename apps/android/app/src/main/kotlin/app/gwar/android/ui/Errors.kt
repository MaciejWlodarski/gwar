package app.gwar.android.ui

import androidx.compose.runtime.Composable
import androidx.compose.ui.res.stringResource
import app.gwar.android.R
import app.gwar.core.AddressError
import app.gwar.core.AppException
import app.gwar.core.CloseReason

@Composable
fun describe(error: AppException): String = when (error) {
    is AppException.Address -> when (error.reason) {
        AddressError.EMPTY -> stringResource(R.string.error_address_empty)
        AddressError.INVALID -> stringResource(R.string.error_address_invalid)
        AddressError.SCHEME -> stringResource(R.string.error_address_scheme)
        AddressError.INSECURE -> stringResource(R.string.error_address_insecure)
    }
    is AppException.ServerIdentityChanged -> stringResource(R.string.identity_changed_title)
    is AppException.Refused -> when (error.code) {
        "wrong_password" -> stringResource(R.string.error_wrong_password)
        "banned" -> stringResource(R.string.error_banned)
        "channel_full" -> stringResource(R.string.error_channel_full)
        "forbidden" -> stringResource(R.string.error_forbidden)
        "rate_limited" -> stringResource(R.string.error_rate_limited)
        else -> stringResource(R.string.error_refused, error.detail)
    }
    is AppException.Timeout -> stringResource(R.string.error_timeout)
    is AppException.NotConnected -> stringResource(R.string.error_not_connected)
    is AppException.Network -> stringResource(R.string.error_network, error.detail)
    is AppException.Protocol -> stringResource(R.string.error_protocol, error.detail)
    is AppException.Storage -> stringResource(R.string.error_storage, error.detail)
}

@Composable
fun describe(reason: CloseReason): String = when (reason) {
    CloseReason.ConnectionLost -> stringResource(R.string.closed_connection_lost)
    CloseReason.Timeout -> stringResource(R.string.closed_timeout)
    is CloseReason.Kicked -> reason.reason?.let { stringResource(R.string.closed_kicked_reason, reason.by, it) }
        ?: stringResource(R.string.closed_kicked, reason.by)
    is CloseReason.Banned -> reason.reason?.let { stringResource(R.string.closed_banned_reason, reason.by, it) }
        ?: stringResource(R.string.closed_banned, reason.by)
    is CloseReason.Removed -> stringResource(R.string.closed_removed, reason.by)
    CloseReason.ServerShutdown -> stringResource(R.string.closed_shutdown)
    CloseReason.Replaced -> stringResource(R.string.closed_replaced)
}
