package com.futo.fcast.receiver.views

import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonColors
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.futo.fcast.receiver.BuildConfig
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.composables.Spinner
import com.futo.fcast.receiver.composables.interFontFamily
import com.futo.fcast.receiver.models.MainActivityViewModel

@Composable
fun UpdateView(viewModel: MainActivityViewModel, modifier: Modifier = Modifier) {
    if (viewModel.updateAvailable) {
        @Suppress("SENSELESS_COMPARISON")
        if (!BuildConfig.IS_PLAYSTORE_VERSION) {
            Column(
                modifier = modifier,
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    if (viewModel.updateStatus != null) {
                        Text(
                            text = viewModel.updateStatus!!,
                            modifier = Modifier
                                .padding(horizontal = 30.dp, vertical = 10.dp),
                            color = Color.White,
                            fontSize = 14.sp,
                            fontFamily = interFontFamily,
                            fontWeight = FontWeight.Normal,
                            textAlign = TextAlign.Center,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis
                        )
                        if (viewModel.updateResultSuccessful != null) {
                            Image(
                                painter = if (viewModel.updateResultSuccessful!!)
                                    painterResource(R.drawable.ic_update_success)
                                else painterResource(R.drawable.ic_update_fail),
                                contentDescription = null,
                                modifier = Modifier
                                    .size(30.dp)
                            )
                        }
                    }
                }

                if (!viewModel.updating) {
                    Button(
                        onClick = viewModel::update,
                        modifier = Modifier,
                        enabled = true,
                        colors = ButtonDefaults.buttonColors(
                            containerColor = Color(0xFF2D63ED)
                        )
                    ) {
                        Text(
                            text = stringResource(R.string.update),
                            modifier = Modifier,
                            color = Color.White,
                            fontSize = 14.sp,
                            fontFamily = interFontFamily,
                            fontWeight = FontWeight.Normal,
                            textAlign = TextAlign.Center,
                        )
                    }
                }
                else {
                    Row(
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        Text(
                            text = viewModel.updateProgress,
                            modifier = Modifier.padding(end = 8.dp),
                            color = Color.White,
                            fontSize = 12.sp,
                            fontFamily = interFontFamily,
                            fontWeight = FontWeight.Normal,
                            textAlign = TextAlign.Center,
                        )
                        Spinner(
                            modifier = Modifier
                                .size(30.dp),
                            R.drawable.ic_update_animated
                        )
                    }
                }
            }
        }
    }
}

@Preview
@Composable
fun UpdateViewPreview() {
    val viewModel = MainActivityViewModel()
    viewModel.updateAvailable = true
    viewModel.updating = false
    viewModel.updateStatus = stringResource(R.string.update_status)
    UpdateView(viewModel)
}

@Preview
@Composable
fun UpdateViewPreviewUpdating() {
    val viewModel = MainActivityViewModel()
    viewModel.updateAvailable = true
    viewModel.updating = true
    viewModel.updateStatus = stringResource(R.string.downloading_update)
    viewModel.updateProgress = "50%"
    UpdateView(viewModel)
}

@Preview
@Composable
fun UpdateViewPreviewUpdateSuccess() {
    val viewModel = MainActivityViewModel()
    viewModel.updateAvailable = false
    viewModel.updating = false
    viewModel.updateStatus = stringResource(R.string.success)
    viewModel.updateResultSuccessful = true
    UpdateView(viewModel)
}

@Preview
@Composable
fun UpdateViewPreviewUpdateFail() {
    val viewModel = MainActivityViewModel()
    viewModel.updateAvailable = false
    viewModel.updating = false
    viewModel.updateStatus = stringResource(R.string.failed_to_update_with_error)
    viewModel.updateResultSuccessful = false
    UpdateView(viewModel)
}

